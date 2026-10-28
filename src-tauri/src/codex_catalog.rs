//! Bounded, read-only history queries. Each request owns and reaps its server.
use super::*;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogQuery {
    #[serde(default)]
    pub query: String,
    pub cwd: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSession {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPage {
    pub sessions: Vec<CatalogSession>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewMessage {
    pub id: String,
    pub role: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPreview {
    pub messages: Vec<PreviewMessage>,
    pub next_cursor: Option<String>,
    pub unavailable_reason: Option<String>,
}

fn with_reader<T>(read: impl FnOnce(&mut Rpc<TcpStream>) -> Result<T, String>) -> Result<T, String> {
    // No blocking admission queue: frontends coalesce pending intentions, and a
    // second caller cannot accumulate workers or additional protocol processes.
    let _request = CATALOG_REQUEST.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => "다른 세션 조회가 진행 중입니다. 잠시 후 다시 시도하세요.".to_owned(),
        std::sync::TryLockError::Poisoned(_) => "세션 조회 잠금이 손상되었습니다. 앱을 다시 시작하세요.".to_owned(),
    })?;
    let directory = env::temp_dir().canonicalize().map_err(|e| format!("Cannot open catalog process working directory: {e}"))?;
    let (server, mut rpc) = launch_server(&directory, None)?;
    let result = read(&mut rpc);
    // Closing is best-effort; always shut down the transport and reap the owned
    // Job even after an RPC error. Do not retain a CLOSE_WAIT socket or wait for
    // a peer's close acknowledgement during resource cleanup.
    let _ = rpc.ws.close(None);
    let _ = rpc.ws.get_ref().shutdown(Shutdown::Both);
    drop(rpc);
    drop(server);
    result
}

fn cursor(value: &Value, previous: Option<&str>) -> Result<Option<String>, String> {
    match value.get("nextCursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(next)) if !next.is_empty() && next.len() <= 4096 && previous != Some(next.as_str()) => Ok(Some(next.clone())),
        _ => Err("Codex returned an invalid or repeated catalog cursor".into()),
    }
}

fn validate_cursor(value: Option<&str>) -> Result<(), String> {
    if value.is_some_and(|value| value.is_empty() || value.len() > 4096) {
        return Err("Session catalog cursor must contain 1–4096 bytes".into());
    }
    Ok(())
}

fn bounded_text(text: &str, maximum: usize) -> String {
    if text.len() <= maximum { return text.to_owned(); }
    let mut end = maximum.saturating_sub('…'.len_utf8());
    while end > 0 && !text.is_char_boundary(end) { end -= 1; }
    let mut result = text[..end].to_owned();
    if maximum >= '…'.len_utf8() { result.push('…'); }
    result
}

fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)] {
        if let Some(value) = value.strip_prefix("\\\\?\\UNC\\") { return format!("\\\\{value}"); }
        if let Some(value) = value.strip_prefix("\\\\?\\") { return value.to_owned(); }
    }
    value.into_owned()
}

fn catalog_session(thread: &Value) -> Result<CatalogSession, String> {
    let id = thread["id"].as_str().ok_or("Codex catalog entry has no thread ID")?;
    Uuid::parse_str(id).map_err(|_| "Codex catalog entry has an invalid thread UUID")?;
    let cwd = thread["cwd"].as_str().filter(|cwd| !cwd.is_empty() && cwd.len() <= 128 * 1024).ok_or("Codex catalog entry has no bounded working directory")?;
    let title = thread["name"].as_str().filter(|name| !name.trim().is_empty())
        .or_else(|| thread["preview"].as_str().filter(|text| !text.trim().is_empty())).unwrap_or(id);
    Ok(CatalogSession {
        id: id.to_owned(), title: bounded_text(title, 2048), cwd: display_path(Path::new(cwd)),
        created_at: thread["createdAt"].as_i64().ok_or("Codex catalog entry has no creation timestamp")?,
        updated_at: thread["updatedAt"].as_i64().ok_or("Codex catalog entry has no update timestamp")?,
        // A separate history reader cannot establish another process's live
        // state. The frontend can match known owned thread IDs independently.
        status: "unknown".into(),
    })
}

fn parse_page(value: &Value, previous: Option<&str>) -> Result<CatalogPage, String> {
    let rows = value["data"].as_array().ok_or("Codex catalog response has no data array")?;
    if rows.len() > CATALOG_PAGE_SIZE { return Err("Codex catalog response exceeded the requested page size".into()); }
    let sessions = rows.iter().map(catalog_session).collect::<Result<_, _>>()?;
    Ok(CatalogPage { sessions, next_cursor: cursor(value, previous)? })
}

pub fn catalog_list(request: CatalogQuery) -> Result<CatalogPage, String> {
    let query = request.query.trim();
    if query.len() > 1024 { return Err("Session search must not exceed 1024 UTF-8 bytes".into()); }
    validate_cursor(request.cursor.as_deref())?;
    let mut params = json!({"limit":CATALOG_PAGE_SIZE,"sortKey":"updated_at","sortDirection":"desc","useStateDbOnly":true});
    if !query.is_empty() { params["searchTerm"] = json!(query); }
    if let Some(value) = &request.cursor { params["cursor"] = json!(value); }
    let selected_directory = match request.cwd.as_deref() {
        Some(value) => {
            if value.is_empty() || value.len() > 128 * 1024 || !Path::new(value).is_absolute() { return Err("Project filter must be an absolute directory path".into()); }
            let canonical = Path::new(value).canonicalize().map_err(|e| format!("Cannot open selected project: {e}"))?;
            if !canonical.is_dir() { return Err("Selected project must be a directory".into()); }
            // Codex records both normal Windows paths and verbatim paths. Its
            // cwd filter is exact, so query both spellings without a broad scan.
            let mut spellings = vec![value.to_owned(), canonical.to_string_lossy().into_owned(), display_path(&canonical)];
            #[cfg(windows)] {
                let forward = spellings.iter().map(|path| path.replace('\\', "/")).collect::<Vec<_>>();
                spellings.extend(forward);
            }
            spellings.sort(); spellings.dedup();
            params["cwd"] = json!(spellings);
            Some(canonical)
        }
        None => None,
    };
    with_reader(|rpc| {
        let page = parse_page(&rpc.call("thread/list", params)?, request.cursor.as_deref())?;
        if let Some(expected) = selected_directory {
            for session in &page.sessions {
                let actual = Path::new(&session.cwd).canonicalize().map_err(|e| format!("Cannot verify filtered session directory: {e}"))?;
                if actual != expected { return Err("Codex returned a session outside the selected project".into()); }
            }
        }
        Ok(page)
    })
}

fn message_text(item: &Value) -> Result<Option<(String, String)>, String> {
    match item["type"].as_str() {
        Some("agentMessage") => Ok(Some(("assistant".into(), item["text"].as_str().ok_or("Codex assistant message has no text")?.to_owned()))),
        Some("userMessage") => {
            let content = item["content"].as_array().ok_or("Codex user message has no content array")?;
            let mut text = String::new();
            for part in content {
                if part["type"].as_str() == Some("text") {
                    let part = part["text"].as_str().ok_or("Codex user text item has no text")?;
                    if !text.is_empty() { text.push('\n'); }
                    let remaining = PREVIEW_TEXT_BYTES.saturating_sub(text.len());
                    text.push_str(&bounded_text(part, remaining));
                    if text.len() >= PREVIEW_TEXT_BYTES { break; }
                }
            }
            if text.is_empty() { text = "[텍스트가 없는 사용자 메시지]".into(); }
            Ok(Some(("user".into(), text)))
        }
        _ => Ok(None),
    }
}

fn parse_preview(value: &Value, previous: Option<&str>) -> Result<CatalogPreview, String> {
    let items = value["data"].as_array().ok_or("Codex preview response has no data array")?;
    if items.len() > CATALOG_PAGE_SIZE { return Err("Codex preview exceeded the requested item count".into()); }
    let mut messages = Vec::new();
    let mut bytes = 0usize;
    for entry in items {
        let item = entry.get("item").ok_or("Codex preview entry has no item")?;
        if let Some((role, text)) = message_text(item)? {
            let id = item["id"].as_str().filter(|id| !id.is_empty() && id.len() <= 4096).ok_or("Codex preview message has no bounded ID")?;
            // Share the page budget across the maximum possible message count.
            // This keeps every returned item represented instead of consuming
            // its cursor and silently losing older messages after one huge item.
            let text = bounded_text(&text, (PREVIEW_TEXT_BYTES / CATALOG_PAGE_SIZE).min(PREVIEW_TEXT_BYTES.saturating_sub(bytes)));
            bytes += text.len();
            messages.push(PreviewMessage { id: id.into(), role, text });
        }
    }
    // The backend page is newest-first; render each page chronologically, with
    // an older page prepended by the UI when the user explicitly asks for it.
    messages.reverse();
    let unavailable_reason = if messages.is_empty() { Some("이 페이지에는 사용자 메시지나 답변이 없습니다. 이전 대화를 더 불러올 수 있습니다.".into()) } else { None };
    Ok(CatalogPreview { messages, next_cursor: cursor(value, previous)?, unavailable_reason })
}

pub fn catalog_preview(thread_id: &str, previous: Option<String>) -> Result<CatalogPreview, String> {
    Uuid::parse_str(thread_id).map_err(|_| "Preview requires an exact thread UUID")?;
    validate_cursor(previous.as_deref())?;
    with_reader(|rpc| {
        let params = json!({"threadId":thread_id,"limit":CATALOG_PAGE_SIZE,"sortDirection":"desc","cursor":previous.as_deref()});
        parse_preview(&rpc.call("thread/items/list", params)?, previous.as_deref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_page_validates_identity_and_never_claims_live_ownership() {
        let id = Uuid::new_v4().to_string();
        let row = json!({"id":id,"cwd":"/project","name":"<script>title</script>","createdAt":1,"updatedAt":2,"status":{"type":"active"}});
        let page = parse_page(&json!({"data":[row],"nextCursor":"next"}), None).unwrap();
        assert_eq!(page.sessions[0].id, id);
        assert_eq!(page.sessions[0].status, "unknown");
        assert_eq!(page.next_cursor.as_deref(), Some("next"));
        assert!(parse_page(&json!({"data":[{"id":"not-a-uuid"}]}), None).is_err());
        assert!(parse_page(&json!({"data":[],"nextCursor":"same"}), Some("same")).is_err());
        assert!(parse_page(&json!({"data":[],"nextCursor":12}), None).is_err());
    }
    #[test]
    fn preview_omits_tools_preserves_unicode_and_reverses_descending_pages() {
        let value = json!({"data":[
            {"item":{"type":"agentMessage","id":"answer","text":"한글 답변"}},
            {"item":{"type":"commandExecution","id":"tool","command":"private command"}},
            {"item":{"type":"userMessage","id":"question","content":[{"type":"text","text":"질문"},{"type":"text","text":"두 번째 줄"}]}}
        ],"nextCursor":"older"});
        let page = parse_preview(&value, None).unwrap();
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].id, "question");
        assert_eq!(page.messages[0].text, "질문\n두 번째 줄");
        assert_eq!(page.messages[1].role, "assistant");
        assert_eq!(page.messages[1].text, "한글 답변");
        assert_eq!(page.next_cursor.as_deref(), Some("older"));
        assert!(parse_preview(&json!({"data":[{"item":{"type":"agentMessage","id":"a"}}]}), None).is_err());
    }
    #[test]
    fn preview_and_cursor_budgets_are_enforced_before_returning_ipc_data() {
        let long = "한😀".repeat(PREVIEW_TEXT_BYTES);
        let page = parse_preview(&json!({"data":[{"item":{"type":"agentMessage","id":"a","text":long}},{"item":{"type":"agentMessage","id":"b","text":"older"}}]}), None).unwrap();
        assert!(page.messages.iter().map(|message| message.text.len()).sum::<usize>() <= PREVIEW_TEXT_BYTES);
        assert!(page.messages.last().unwrap().text.ends_with('…'));
        assert!(validate_cursor(Some(&"x".repeat(4097))).is_err());
        assert!(validate_cursor(Some("")).is_err());
        let rows = vec![json!({"item":{"type":"agentMessage","id":"x","text":"x"}}); 51];
        assert!(parse_preview(&json!({"data":rows}), None).is_err());
        assert!(parse_preview(&json!({"data":[],"nextCursor":"same"}), Some("same")).is_err());
    }
}
