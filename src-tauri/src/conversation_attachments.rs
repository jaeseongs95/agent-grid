//! Native attachment selection and app-owned clipboard image lifetimes.
use serde::Serialize;
use std::{collections::{BTreeMap, BTreeSet}, fs, io::Write, path::{Path, PathBuf}, sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex}};

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_SESSION_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_SESSION_FILES: usize = 64;
const MAX_ATTACHMENTS: usize = 32;

#[derive(Clone, Debug, Serialize)]
pub struct ConversationAttachment {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub path: String,
    pub name: String,
}

#[derive(Clone, Default)]
pub struct ConversationAttachments {
    busy: Arc<AtomicBool>,
    staged: Arc<Mutex<Staging>>,
}
struct Staging { root: PathBuf, sessions: BTreeMap<String, StagedSession> }
impl Default for Staging {
    fn default() -> Self { Self { root: std::env::temp_dir().join("opendiy-conversation-attachments-v1"), sessions: BTreeMap::new() } }
}
struct StagedSession { directory: PathBuf, files: BTreeMap<String, StagedFile> }
struct StagedFile { bytes: usize, retained: Option<uuid::Uuid> }
#[derive(Debug)]
pub struct RetentionReceipt { session_id: String, claims: Vec<(String, uuid::Uuid)> }
struct BusyPicker(Arc<AtomicBool>);
impl Drop for BusyPicker { fn drop(&mut self) { self.0.store(false, Ordering::Release); } }

impl ConversationAttachments {
    #[cfg(test)]
    pub(crate) fn test_store(root: PathBuf) -> Self {
        Self { staged: Arc::new(Mutex::new(Staging { root, sessions: BTreeMap::new() })), ..Self::default() }
    }

    pub fn choose(&self, owner: isize, language: crate::ui_language::UiLanguage) -> Result<Vec<ConversationAttachment>, String> {
        self.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "첨부 파일 선택창이 이미 열려 있습니다.")?;
        let _busy = BusyPicker(self.busy.clone());
        std::thread::Builder::new().name("conversation-attachments".into())
            .spawn(move || choose_files(owner, language)).map_err(|e| format!("첨부 파일 선택창 시작 실패: {e}"))?
            .join().map_err(|_| "첨부 파일 선택 작업이 비정상 종료되었습니다.".to_owned())?
    }

    pub fn stage_image(&self, session_id: &str, data_url: &str) -> Result<ConversationAttachment, String> {
        uuid::Uuid::parse_str(session_id).map_err(|_| "첨부 파일의 세션 식별자가 올바르지 않습니다.")?;
        let (extension, encoded) = if let Some(data) = data_url.strip_prefix("data:image/png;base64,") { ("png", data) }
            else if let Some(data) = data_url.strip_prefix("data:image/jpeg;base64,") { ("jpg", data) }
            else { return Err("클립보드 이미지는 PNG 또는 JPEG 형식이어야 합니다.".into()); };
        // ponytail: one bounded decode at a time; add an admission pool only if
        // measured clipboard throughput requires concurrent image decoding.
        let mut staged = self.staged.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => "다른 이미지 저장 작업이 진행 중입니다. 잠시 후 다시 시도하세요.",
            std::sync::TryLockError::Poisoned(_) => "임시 첨부 파일 상태 잠금 실패",
        })?;
        let bytes = decode_base64(encoded)?;
        // The OS lock covers the persistent disk budget and the subsequent write,
        // including other app processes. Closing the handle releases the lock.
        let _disk_lock = staging_lock(&staged.root)?;
        check_disk_budget(&staged.root, bytes.len())?;
        validate_image(&bytes, extension)?;
        if let Some(session) = staged.sessions.get(session_id) {
            if session.files.len() >= MAX_SESSION_FILES || session.files.values().map(|file| file.bytes).sum::<usize>() + bytes.len() > MAX_SESSION_BYTES {
                return Err("세션의 임시 이미지 상한(64개 또는 64MiB)을 초과했습니다.".into());
            }
        } else {
            let directory = staged.root.join(uuid::Uuid::new_v4().to_string());
            fs::create_dir(&directory).map_err(|e| format!("임시 이미지 폴더 생성 실패: {e}"))?;
            staged.sessions.insert(session_id.to_owned(), StagedSession { directory, files: BTreeMap::new() });
        }
        let session = staged.sessions.get_mut(session_id).ok_or("임시 이미지 세션을 찾을 수 없습니다.")?;
        let name = format!("clipboard-{}.{}", uuid::Uuid::new_v4(), extension);
        let path = session.directory.join(&name);
        let path_text = path.to_str().ok_or("임시 이미지 경로가 올바른 유니코드가 아닙니다.")?.to_owned();
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("임시 이미지 생성 실패: {e}"))?;
        // Record ownership before writing so a partial write is still reclaimed.
        session.files.insert(path_text.clone(), StagedFile { bytes: bytes.len(), retained: None });
        if let Err(error) = file.write_all(&bytes) {
            drop(file);
            match fs::remove_file(&path) {
                Ok(()) => { session.files.remove(&path_text); }
                Err(cleanup) => return Err(format!("임시 이미지 저장 실패: {error}; 부분 파일 정리 실패: {cleanup}")),
            }
            return Err(format!("임시 이미지 저장 실패: {error}"));
        }
        Ok(ConversationAttachment { kind: "localImage", path: path_text, name })
    }

    /// Call after generation/admission checks, before submitting paths to the runtime.
    /// Dropping the receipt preserves files, including ambiguous failures/cancellation.
    pub fn retain_for_send(&self, session_id: &str, paths: &[String]) -> Result<RetentionReceipt, String> {
        let mut staged = self.staged.lock().map_err(|_| "임시 첨부 파일 상태 잠금 실패")?;
        // A staged file from another session must never be borrowed by this one.
        for (owner, session) in &staged.sessions {
            if owner != session_id && paths.iter().any(|path| session.files.contains_key(path)) {
                return Err("다른 세션의 임시 첨부 파일을 전송할 수 없습니다.".into());
            }
        }
        let mut receipt = RetentionReceipt { session_id: session_id.to_owned(), claims: Vec::new() };
        if let Some(session) = staged.sessions.get_mut(session_id) {
            for path in paths.iter().collect::<BTreeSet<_>>() {
                if let Some(file) = session.files.get_mut(path) {
                    let token = uuid::Uuid::new_v4();
                    if file.retained.is_none() { receipt.claims.push((path.clone(), token)); }
                    // Even an already-retained reference fences older rollback receipts.
                    file.retained = Some(token);
                }
            }
        }
        Ok(receipt)
    }

    /// Backend only: use exclusively for an authoritative no-side-effect rejection.
    /// Does not delete files and cannot undo prior or subsequent send retention.
    pub fn rollback_known_rejection(&self, session_id: &str, receipt: RetentionReceipt) -> Result<(), String> {
        if receipt.session_id != session_id { return Err("첨부 파일 보존 영수증의 세션이 일치하지 않습니다.".into()); }
        let mut staged = self.staged.lock().map_err(|_| "임시 첨부 파일 상태 잠금 실패")?;
        if let Some(session) = staged.sessions.get_mut(session_id) {
            for (path, token) in receipt.claims {
                if let Some(file) = session.files.get_mut(&path) {
                    if file.retained == Some(token) { file.retained = None; }
                }
            }
        }
        Ok(())
    }

    /// Draft removal only. Never deletes paths not created and tracked here.
    pub fn cleanup_staged(&self, session_id: &str, path: &str) -> Result<(), String> {
        let mut staged = self.staged.lock().map_err(|_| "임시 첨부 파일 상태 잠금 실패")?;
        let session = staged.sessions.get_mut(session_id).ok_or("해당 세션이 소유한 임시 첨부 파일이 없습니다.")?;
        let file = session.files.get(path).ok_or("앱이 소유한 임시 첨부 파일이 아닙니다.")?;
        if file.retained.is_some() { return Err("전송에 사용된 이미지는 대화 이력 보존을 위해 자동 삭제되지 않습니다.".into()); }
        remove_owned_file(path)?;
        session.files.remove(path);
        Ok(())
    }

    /// End a composer/session lifetime. Submitted paths remain valid for shared
    /// runtimes and resumed history, even after the local PTY has exited.
    pub fn cleanup_session(&self, session_id: &str) -> Result<(), String> {
        let mut staged = self.staged.lock().map_err(|_| "임시 첨부 파일 상태 잠금 실패")?;
        if let Some(session) = staged.sessions.get_mut(session_id) { session.cleanup()?; }
        if staged.sessions.get(session_id).is_some_and(|session| session.files.is_empty()) { staged.sessions.remove(session_id); }
        Ok(())
    }

    /// Reclaim unsubmitted drafts only. Never deletes submitted history images.
    pub fn cleanup_all(&self) -> Result<(), String> {
        let mut staged = self.staged.lock().map_err(|_| "임시 첨부 파일 상태 잠금 실패")?;
        let mut errors = Vec::new();
        staged.sessions.retain(|_, session| match session.cleanup() { Ok(()) => !session.files.is_empty(), Err(error) => { errors.push(error); true } });
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }
}

impl StagedSession {
    fn cleanup(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        self.files.retain(|path, file| file.retained.is_some() || match remove_owned_file(path) { Ok(()) => false, Err(error) => { errors.push(error); true } });
        if self.files.is_empty() {
            if let Err(error) = fs::remove_dir(&self.directory) {
                if error.kind() != std::io::ErrorKind::NotFound { errors.push(format!("임시 이미지 폴더 정리 실패: {error}")); }
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        for session in self.sessions.values_mut() {
            if let Err(error) = session.cleanup() { eprintln!("임시 첨부 파일 종료 정리 실패: {error}"); }
        }
    }
}

fn plain_metadata(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| format!("임시 이미지 저장소 검사 실패: {e}"))?;
    #[cfg(windows)]
    let redirected = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let redirected = metadata.file_type().is_symlink();
    if redirected { return Err("임시 이미지 저장소의 링크 또는 재분석 지점은 허용되지 않습니다.".into()); }
    Ok(metadata)
}

fn staging_lock(root: &Path) -> Result<fs::File, String> {
    match fs::create_dir(root) {
        Ok(()) => (), Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(format!("임시 이미지 저장소 생성 실패: {error}")),
    }
    if !plain_metadata(root)?.is_dir() { return Err("임시 이미지 저장소가 폴더가 아닙니다.".into()); }
    let path = root.join(".staging.lock");
    if path.try_exists().map_err(|e| format!("임시 이미지 잠금 검사 실패: {e}"))? { plain_metadata(&path)?; }
    let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
        .map_err(|e| format!("임시 이미지 잠금 파일 열기 실패: {e}"))?;
    file.try_lock().map_err(|e| format!("다른 앱의 임시 이미지 저장 작업과 경합했거나 저장소 잠금에 실패했습니다: {e}"))?;
    Ok(file)
}

fn check_disk_budget(root: &Path, additional: usize) -> Result<(), String> {
    fn visit(directory: &Path, depth: u8, entries: &mut usize, bytes: &mut u64) -> Result<(), String> {
        for entry in fs::read_dir(directory).map_err(|e| format!("임시 이미지 용량 조회 실패: {e}"))? {
            *entries += 1;
            if *entries > 16384 { return Err("임시 이미지 저장소의 항목 수 상한을 초과했습니다.".into()); }
            let entry = entry.map_err(|e| format!("임시 이미지 항목 조회 실패: {e}"))?;
            let metadata = plain_metadata(&entry.path())?;
            if metadata.is_dir() {
                if depth == 0 { return Err("임시 이미지 저장소의 폴더 구조가 올바르지 않습니다.".into()); }
                visit(&entry.path(), depth - 1, entries, bytes)?;
            } else if metadata.is_file() {
                *bytes = bytes.checked_add(metadata.len()).ok_or("임시 이미지 용량 합계가 범위를 초과했습니다.")?;
                if *bytes > MAX_TOTAL_BYTES as u64 { return Err("앱의 임시 이미지 용량 상한 256MiB를 초과했습니다.".into()); }
            } else { return Err("임시 이미지 저장소에 지원하지 않는 항목이 있습니다.".into()); }
        }
        Ok(())
    }
    let (mut entries, mut bytes) = (0, additional as u64);
    if bytes > MAX_TOTAL_BYTES as u64 { return Err("앱의 임시 이미지 용량 상한 256MiB를 초과했습니다.".into()); }
    visit(root, 1, &mut entries, &mut bytes)
}

fn remove_owned_file(path: &str) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()), Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("임시 이미지 정리 실패: {error}")),
    }
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    let input = encoded.as_bytes();
    if input.is_empty() || input.len() % 4 != 0 || input.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err("이미지 인코딩이 올바르지 않거나 10MiB 상한을 초과했습니다.".into());
    }
    let value = |byte| match byte { b'A'..=b'Z' => Some(byte - b'A'), b'a'..=b'z' => Some(byte - b'a' + 26), b'0'..=b'9' => Some(byte - b'0' + 52), b'+' => Some(62), b'/' => Some(63), _ => None };
    let mut bytes = Vec::with_capacity((input.len() / 4 * 3).min(MAX_IMAGE_BYTES));
    for (index, chunk) in input.chunks_exact(4).enumerate() {
        let last = (index + 1) * 4 == input.len();
        let a = value(chunk[0]).ok_or("이미지 base64 문자가 올바르지 않습니다.")?;
        let b = value(chunk[1]).ok_or("이미지 base64 문자가 올바르지 않습니다.")?;
        let c = if chunk[2] == b'=' && last { 0 } else { value(chunk[2]).ok_or("이미지 base64 채움이 올바르지 않습니다.")? };
        let d = if chunk[3] == b'=' && last { 0 } else { value(chunk[3]).ok_or("이미지 base64 채움이 올바르지 않습니다.")? };
        if (chunk[2] == b'=' && (chunk[3] != b'=' || b & 15 != 0)) || (chunk[3] == b'=' && chunk[2] != b'=' && c & 3 != 0) {
            return Err("이미지 base64 채움 비트가 올바르지 않습니다.".into());
        }
        bytes.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' { bytes.push((b << 4) | (c >> 2)); }
        if chunk[3] != b'=' { bytes.push((c << 6) | d); }
    }
    if bytes.len() > MAX_IMAGE_BYTES { return Err("이미지 10MiB 상한을 초과했습니다.".into()); }
    Ok(bytes)
}

fn selected_attachment(path: &Path) -> Result<ConversationAttachment, String> {
    if !path.is_absolute() { return Err("첨부 파일 경로는 절대 경로여야 합니다.".into()); }
    let path = path.canonicalize().map_err(|e| format!("첨부 파일을 열 수 없습니다: {e}"))?;
    if !path.is_file() { return Err("첨부 대상은 파일이어야 합니다.".into()); }
    let name = path.file_name().and_then(|name| name.to_str()).ok_or("첨부 파일명이 올바른 유니코드가 아닙니다.")?.to_owned();
    let kind = if path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| ["png", "jpg", "jpeg", "webp", "gif"].iter().any(|image| extension.eq_ignore_ascii_case(image))) { "localImage" } else { "file" };
    let path_text = path.to_str().ok_or("첨부 파일 경로가 올바른 유니코드가 아닙니다.")?.to_owned();
    #[cfg(windows)] {
        let path_text = if let Some(unc) = path_text.strip_prefix(r"\\?\UNC\") { format!(r"\\{unc}") }
            else if let Some(local) = path_text.strip_prefix(r"\\?\") { local.to_owned() } else { path_text };
        return Ok(ConversationAttachment { kind, path: path_text, name });
    }
    #[cfg(not(windows))]
    Ok(ConversationAttachment { kind, path: path_text, name })
}

fn selected_paths(buffer: &[u16]) -> Result<Vec<PathBuf>, String> {
    let mut parts = Vec::new();
    let mut offset = 0;
    loop {
        let end = buffer.get(offset..).and_then(|tail| tail.iter().position(|unit| *unit == 0)).ok_or("첨부 파일 선택 결과가 종료되지 않았습니다.")?;
        if end == 0 { break; }
        #[cfg(windows)]
        let part = { use std::os::windows::ffi::OsStringExt; PathBuf::from(std::ffi::OsString::from_wide(&buffer[offset..offset + end])) };
        #[cfg(not(windows))]
        let part = PathBuf::from(String::from_utf16(&buffer[offset..offset + end]).map_err(|_| "첨부 파일 경로가 올바르지 않습니다.")?);
        parts.push(part);
        if parts.len() > MAX_ATTACHMENTS + 1 { return Err("한 번에 첨부할 파일은 최대 32개입니다.".into()); }
        offset += end + 1;
    }
    if parts.len() <= 1 { return Ok(parts); }
    let directory = parts.remove(0);
    parts.into_iter().map(|name| {
        if name.components().count() != 1 || !matches!(name.components().next(), Some(std::path::Component::Normal(_))) { return Err("첨부 파일 선택 결과에 잘못된 파일명이 있습니다.".into()); }
        Ok(directory.join(name))
    }).collect()
}

#[cfg(windows)]
fn choose_files(owner: isize, language: crate::ui_language::UiLanguage) -> Result<Vec<ConversationAttachment>, String> {
    use windows_sys::Win32::{Foundation::HWND, System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE}, UI::Controls::Dialogs::{CommDlgExtendedError, GetOpenFileNameW, OPENFILENAMEW, OFN_ALLOWMULTISELECT, OFN_DONTADDTORECENT, OFN_ENABLESIZING, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST}};
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self) { unsafe { CoUninitialize(); } } }
    let initialized = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
    if initialized < 0 { return Err(format!("첨부 파일 선택창 초기화 실패: {initialized:#010x}")); }
    let _apartment = Apartment;
    let title: Vec<u16> = language.text("대화에 첨부할 파일 선택", "Select conversation attachments").encode_utf16().chain(Some(0)).collect();
    let filter: Vec<u16> = language.text("모든 파일 (*.*)\0*.*\0이미지 (*.png;*.jpg;*.jpeg;*.webp;*.gif)\0*.png;*.jpg;*.jpeg;*.webp;*.gif\0\0", "All files (*.*)\0*.*\0Images (*.png;*.jpg;*.jpeg;*.webp;*.gif)\0*.png;*.jpg;*.jpeg;*.webp;*.gif\0\0").encode_utf16().collect();
    let mut buffer = vec![0u16; 65536];
    let mut dialog = OPENFILENAMEW { lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32, hwndOwner: owner as HWND, lpstrFilter: filter.as_ptr(), nFilterIndex: 1, lpstrFile: buffer.as_mut_ptr(), nMaxFile: buffer.len() as u32, lpstrTitle: title.as_ptr(), Flags: OFN_EXPLORER | OFN_ALLOWMULTISELECT | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT | OFN_HIDEREADONLY | OFN_ENABLESIZING, ..Default::default() };
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 { Ok(Vec::new()) } else { Err(format!("첨부 파일 선택창 오류: {error:#010x}")) };
    }
    selected_paths(&buffer)?.iter().map(|path| selected_attachment(path)).collect()
}
#[cfg(not(windows))]
fn choose_files(_: isize, _: crate::ui_language::UiLanguage) -> Result<Vec<ConversationAttachment>, String> { Err("기본 첨부 파일 선택창은 Windows에서만 지원됩니다.".into()) }

fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 || width > 16384 || height > 16384 || u64::from(width) * u64::from(height) > 16_777_216 {
        return Err("이미지는 가로·세로 16384 및 전체 16777216 화소 이하여야 합니다.".into());
    }
    Ok(())
}

// Inspect dimensions before asking the native decoder to allocate a surface.
// The decoder remains authoritative for image validity and decoded dimensions.
fn image_dimensions(bytes: &[u8], extension: &str) -> Result<(u32, u32), String> {
    if extension == "png" && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(8..16) == Some(b"\0\0\0\rIHDR") {
        let dimensions = bytes.get(16..24).ok_or("PNG 크기 정보가 잘렸습니다.")?;
        let width = u32::from_be_bytes(dimensions[..4].try_into().map_err(|_| "PNG 너비 정보가 올바르지 않습니다.")?);
        let height = u32::from_be_bytes(dimensions[4..].try_into().map_err(|_| "PNG 높이 정보가 올바르지 않습니다.")?);
        validate_dimensions(width, height)?;
        return Ok((width, height));
    }
    if extension == "jpg" && bytes.starts_with(b"\xff\xd8\xff") {
        let mut offset = 2;
        while offset < bytes.len() {
            if bytes[offset] != 0xff { return Err("JPEG 마커가 올바르지 않습니다.".into()); }
            while bytes.get(offset) == Some(&0xff) { offset += 1; }
            let marker = *bytes.get(offset).ok_or("JPEG 마커가 잘렸습니다.")?;
            offset += 1;
            if marker == 0xda || marker == 0xd9 { break; }
            if marker == 0x01 || (0xd0..=0xd7).contains(&marker) { continue; }
            let length = bytes.get(offset..offset + 2).ok_or("JPEG 구간 길이가 잘렸습니다.")?;
            let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
            if length < 2 || offset + length > bytes.len() { return Err("JPEG 구간 길이가 올바르지 않습니다.".into()); }
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                if length < 8 { return Err("JPEG 크기 정보가 잘렸습니다.".into()); }
                let height = u32::from(u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]));
                let width = u32::from(u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]));
                validate_dimensions(width, height)?;
                return Ok((width, height));
            }
            offset += length;
        }
    }
    Err("이미지 내용에 올바른 PNG 또는 JPEG 크기 정보가 없습니다.".into())
}

#[cfg(windows)]
fn validate_image(bytes: &[u8], extension: &str) -> Result<(), String> {
    use std::ffi::c_void;
    use windows::core::Interface;
    use windows::Win32::System::Com::IStream;
    #[repr(C)] struct StartupInput { version: u32, callback: *mut c_void, suppress_thread: i32, suppress_codecs: i32 }
    #[repr(C)] struct Rect { x: i32, y: i32, width: i32, height: i32 }
    #[repr(C)] struct BitmapData { width: u32, height: u32, stride: i32, format: i32, pixels: *mut c_void, reserved: usize }
    #[link(name = "gdiplus")] unsafe extern "system" {
        fn GdiplusStartup(token: *mut usize, input: *const StartupInput, output: *mut c_void) -> i32;
        fn GdiplusShutdown(token: usize);
        fn GdipCreateBitmapFromStream(stream: *mut c_void, bitmap: *mut *mut c_void) -> i32;
        fn GdipDisposeImage(image: *mut c_void) -> i32;
        fn GdipGetImageWidth(image: *mut c_void, width: *mut u32) -> i32;
        fn GdipGetImageHeight(image: *mut c_void, height: *mut u32) -> i32;
        fn GdipBitmapLockBits(bitmap: *mut c_void, rect: *const Rect, flags: u32, format: i32, data: *mut BitmapData) -> i32;
        fn GdipBitmapUnlockBits(bitmap: *mut c_void, data: *mut BitmapData) -> i32;
    }
    #[link(name = "shlwapi")] unsafe extern "system" { fn SHCreateMemStream(bytes: *const u8, size: u32) -> *mut c_void; }
    struct Gdi(usize); impl Drop for Gdi { fn drop(&mut self) { unsafe { GdiplusShutdown(self.0); } } }
    struct Image(*mut c_void); impl Drop for Image { fn drop(&mut self) { unsafe { GdipDisposeImage(self.0); } } }
    let expected_dimensions = image_dimensions(bytes, extension)?;
    let mut token = 0;
    let input = StartupInput { version: 1, callback: std::ptr::null_mut(), suppress_thread: 0, suppress_codecs: 1 };
    let status = unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) };
    if status != 0 { return Err(format!("Windows 이미지 검증기 초기화 실패: {status}")); }
    let _gdi = Gdi(token);
    let raw_stream = unsafe { SHCreateMemStream(bytes.as_ptr(), bytes.len() as u32) };
    if raw_stream.is_null() { return Err("이미지 검증 스트림 생성 실패".into()); }
    let stream = unsafe { IStream::from_raw(raw_stream) };
    let mut image = std::ptr::null_mut();
    let status = unsafe { GdipCreateBitmapFromStream(stream.as_raw(), &mut image) };
    if status != 0 || image.is_null() {
        if !image.is_null() { unsafe { GdipDisposeImage(image); } }
        return Err(format!("PNG/JPEG 이미지를 디코딩할 수 없습니다: {status}"));
    }
    let image = Image(image);
    let (mut width, mut height) = (0, 0);
    if unsafe { GdipGetImageWidth(image.0, &mut width) } != 0 || unsafe { GdipGetImageHeight(image.0, &mut height) } != 0 {
        return Err("이미지 크기를 읽을 수 없습니다.".into());
    }
    validate_dimensions(width, height)?;
    if (width, height) != expected_dimensions { return Err("이미지 헤더와 디코딩된 크기가 일치하지 않습니다.".into()); }
    // Force pixel decoding while the stream is alive, with a bounded 64MiB surface.
    let mut data = BitmapData { width: 0, height: 0, stride: 0, format: 0, pixels: std::ptr::null_mut(), reserved: 0 };
    let rect = Rect { x: 0, y: 0, width: width as i32, height: height as i32 };
    let status = unsafe { GdipBitmapLockBits(image.0, &rect, 1, 0x26200a, &mut data) };
    if status != 0 { return Err(format!("이미지 화소 디코딩 실패: {status}")); }
    let status = unsafe { GdipBitmapUnlockBits(image.0, &mut data) };
    if status != 0 { return Err(format!("이미지 검증 자원 해제 실패: {status}")); }
    Ok(())
}
#[cfg(not(windows))]
fn validate_image(_: &[u8], _: &str) -> Result<(), String> { Err("클립보드 이미지 검증은 Windows에서만 지원됩니다.".into()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_base64_rejects_malformed_and_oversized_input() {
        assert_eq!(decode_base64("YWJj").unwrap(), b"abc");
        assert_eq!(decode_base64("YQ==").unwrap(), b"a");
        assert_eq!(decode_base64("YWI=").unwrap(), b"ab");
        for input in ["", "a", "YQ=a", "YR==", "YWJ=", "YQ==AAAA", "Y Q=", "===="] { assert!(decode_base64(input).is_err(), "{input}"); }
        assert!(decode_base64(&"A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 4)).is_err());
    }

    #[test]
    fn decoded_image_dimensions_bound_surface_memory_without_integer_overflow() {
        assert!(validate_dimensions(4096, 4096).is_ok());
        assert!(validate_dimensions(16384, 1024).is_ok());
        for (width, height) in [(0, 1), (1, 0), (16385, 1), (1, 16385), (4097, 4096), (u32::MAX, u32::MAX)] {
            assert!(validate_dimensions(width, height).is_err(), "{width}x{height}");
        }
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&4096u32.to_be_bytes());
        png.extend_from_slice(&4096u32.to_be_bytes());
        assert_eq!(image_dimensions(&png, "png").unwrap(), (4096, 4096));
        png[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(image_dimensions(&png, "png").is_err());
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0, 2, 0xff, 0xc0, 0, 8, 8, 0, 2, 0, 3, 1];
        assert_eq!(image_dimensions(&jpeg, "jpg").unwrap(), (3, 2));
        for end in 0..jpeg.len() { assert!(image_dimensions(&jpeg[..end], "jpg").is_err()); }
        assert!(image_dimensions(&jpeg, "png").is_err());
    }

    #[test]
    fn multi_selection_preserves_unicode_and_checks_bounds() {
        let buffer: Vec<u16> = "D:\\자료\0한글.png\0읽기.txt\0\0".encode_utf16().collect();
        let selected = selected_paths(&buffer).unwrap();
        assert_eq!(selected, vec![PathBuf::from("D:\\자료").join("한글.png"), PathBuf::from("D:\\자료").join("읽기.txt")]);
        assert!(selected_paths(&[65]).is_err());
        let too_many: Vec<u16> = format!("D:\\자료\0{}\0", "a\0".repeat(MAX_ATTACHMENTS + 1)).encode_utf16().collect();
        assert!(selected_paths(&too_many).is_err());
    }

    #[test]
    fn selected_images_match_runtime_support_and_bmp_remains_a_file() {
        let root = std::env::temp_dir().join(format!("opendiy-attachment-kind-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        for (extension, expected) in [("PNG", "localImage"), ("jpg", "localImage"), ("jpeg", "localImage"), ("webp", "localImage"), ("gif", "localImage"), ("bmp", "file"), ("BMP", "file"), ("txt", "file")] {
            let path = root.join(format!("한글 첨부.{extension}"));
            fs::write(&path, []).unwrap();
            assert_eq!(selected_attachment(&path).unwrap().kind, expected);
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn staging_rejects_concurrent_work_without_queuing_image_input() {
        let manager = ConversationAttachments::default();
        let guard = manager.staged.lock().unwrap();
        let pending = manager.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender.send(pending.stage_image(&uuid::Uuid::new_v4().to_string(), "data:image/png;base64,YQ==")).unwrap();
        });
        let observed = receiver.recv_timeout(std::time::Duration::from_secs(2));
        drop(guard);
        worker.join().unwrap();
        assert!(matches!(observed, Ok(Err(error)) if error.contains("다른 이미지 저장 작업")));
    }

    #[test]
    fn known_rejection_rolls_back_only_new_unchanged_retention_claims() {
        let root = std::env::temp_dir().join(format!("opendiy-attachment-receipt-test-{}", uuid::Uuid::new_v4()));
        let directory = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&root).unwrap();
        fs::create_dir(&directory).unwrap();
        let manager = ConversationAttachments::test_store(root.clone());
        let id = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let paths: Vec<String> = ["new.png", "previous.png", "racing.png", "mismatch.png"].iter().map(|name| {
            let path = directory.join(name);
            fs::write(&path, [1]).unwrap();
            path.to_str().unwrap().to_owned()
        }).collect();
        manager.staged.lock().unwrap().sessions.insert(id.clone(), StagedSession {
            directory: directory.clone(),
            files: paths.iter().map(|path| (path.clone(), StagedFile { bytes: 1, retained: None })).collect(),
        });
        // Dropping an ambiguous/started receipt never releases its retention.
        drop(manager.retain_for_send(&id, &[paths[1].clone()]).unwrap());
        let rejected = manager.retain_for_send(&id, &[paths[0].clone(), paths[0].clone(), paths[1].clone()]).unwrap();
        manager.rollback_known_rejection(&id, rejected).unwrap();
        manager.cleanup_staged(&id, &paths[0]).unwrap();
        assert!(manager.cleanup_staged(&id, &paths[1]).is_err());
        assert!(manager.retain_for_send(&other, &[paths[1].clone()]).is_err());
        // A later send from another worker fences the older request's receipt.
        let first = manager.retain_for_send(&id, &[paths[2].clone()]).unwrap();
        let concurrent = manager.clone();
        let concurrent_id = id.clone();
        let concurrent_path = paths[2].clone();
        std::thread::spawn(move || drop(concurrent.retain_for_send(&concurrent_id, &[concurrent_path]).unwrap())).join().unwrap();
        manager.rollback_known_rejection(&id, first).unwrap();
        assert!(manager.cleanup_staged(&id, &paths[2]).is_err());
        let mismatched = manager.retain_for_send(&id, &[paths[3].clone()]).unwrap();
        assert!(manager.rollback_known_rejection(&other, mismatched).is_err());
        assert!(manager.cleanup_staged(&id, &paths[3]).is_err());
        manager.cleanup_all().unwrap();
        drop(manager);
        for path in &paths[1..] { assert!(Path::new(path).is_file()); fs::remove_file(path).unwrap(); }
        fs::remove_dir(directory).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn persistent_budget_and_process_lock_are_enforced() {
        const CHILD_ROOT: &str = "OPENDIY_ATTACHMENT_LOCK_TEST_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            assert!(staging_lock(Path::new(&root)).is_err(), "다른 프로세스의 잠금을 획득하면 안 됩니다.");
            return;
        }
        let root = std::env::temp_dir().join(format!("opendiy-attachment-budget-test-{}", uuid::Uuid::new_v4()));
        let lock = staging_lock(&root).unwrap();
        let directory = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&directory).unwrap();
        let old_file = directory.join("previous-run.png");
        fs::write(&old_file, [1, 2, 3]).unwrap();
        assert!(check_disk_budget(&root, MAX_TOTAL_BYTES - 3).is_ok());
        assert!(check_disk_budget(&root, MAX_TOTAL_BYTES - 2).is_err());
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "conversation_attachments::tests::persistent_budget_and_process_lock_are_enforced", "--nocapture"])
            .env(CHILD_ROOT, &root).output().unwrap();
        assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"), "{}", String::from_utf8_lossy(&child.stdout));
        drop(lock);
        let released = staging_lock(&root).unwrap();
        drop(released);
        fs::remove_file(old_file).unwrap();
        fs::remove_dir(directory).unwrap();
        fs::remove_file(root.join(".staging.lock")).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_decoder_and_session_lifetime_preserve_original_files() {
        let root = std::env::temp_dir().join(format!("opendiy-attachment-lifetime-test-{}", uuid::Uuid::new_v4()));
        let manager = ConversationAttachments { staged: Arc::new(Mutex::new(Staging { root: root.clone(), sessions: BTreeMap::new() })), ..ConversationAttachments::default() };
        let id = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        // Generated with the built-in Windows JPEG encoder, one black pixel.
        let jpeg = "data:image/jpeg;base64,/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAABAAEDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwD8qqKKKAP/2Q==";
        assert!(manager.stage_image(&id, "data:image/png;base64,iVBORw0KGgo=").is_err());
        assert!(manager.stage_image("../escape", png).is_err());
        let attachment = manager.stage_image(&id, png).unwrap();
        let directory = Path::new(&attachment.path).parent().unwrap().to_owned();
        let original = std::env::temp_dir().join(format!("한글 원본-{}.txt", uuid::Uuid::new_v4()));
        fs::write(&original, "보존").unwrap();
        let selected = selected_attachment(&original).unwrap();
        assert_eq!(selected.kind, "file");
        assert!(manager.cleanup_staged(&id, &selected.path).is_err());
        assert!(manager.retain_for_send(&other, &[attachment.path.clone()]).is_err());
        manager.retain_for_send(&id, &[attachment.path.clone(), selected.path]).unwrap();
        assert!(manager.cleanup_staged(&id, &attachment.path).is_err());
        assert!(Path::new(&attachment.path).is_file());
        manager.cleanup_session(&id).unwrap();
        assert!(Path::new(&attachment.path).is_file());
        assert_eq!(fs::read_to_string(&original).unwrap(), "보존");
        fs::remove_file(original).unwrap();
        let draft = manager.stage_image(&other, png).unwrap();
        manager.cleanup_staged(&other, &draft.path).unwrap();
        assert!(!Path::new(&draft.path).exists());
        let jpeg_attachment = manager.stage_image(&other, jpeg).unwrap();
        assert!(jpeg_attachment.path.ends_with(".jpg"));
        manager.cleanup_all().unwrap();
        assert!(!Path::new(&jpeg_attachment.path).exists());
        drop(manager);
        assert!(Path::new(&attachment.path).is_file(), "이력에서 참조할 수 있는 전송 이미지는 앱 종료 후에도 보존합니다.");
        // Only this test owns these fixtures; production has no history deletion API.
        fs::remove_file(&attachment.path).unwrap();
        fs::remove_dir(directory).unwrap();
        fs::remove_file(root.join(".staging.lock")).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
