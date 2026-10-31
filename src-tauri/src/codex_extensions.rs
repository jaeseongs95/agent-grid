//! CLI-owned extension state. No credentials, shell commands or arbitrary RPCs
//! cross the renderer boundary. Every short-lived process is owned and reaped.
use super::*;
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

const MAX_ROWS: usize = 4096;
// 실제 카탈로그는 9천 개를 넘는다. CLI 원문 8 MiB 제한도 별도로 유지한다.
const MAX_PLUGIN_ROWS: usize = 16384;
const MAX_IMPORT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMPORT_FILES: usize = 1024;
// ponytail: one admission lock keeps extension mutations/readbacks coherent;
// use per-runtime locks only if multiple independent runtimes become supported.
static EXTENSIONS_REQUEST: Mutex<()> = Mutex::new(());

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPlugin {
    pub plugin_id: String,
    pub name: String,
    pub marketplace_name: String,
    pub version: Option<String>,
    pub enabled: bool,
    pub source: String,
    pub install_policy: Option<String>,
    pub auth_policy: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ExtensionPlugins { pub installed: Vec<ExtensionPlugin>, pub available: Vec<ExtensionPlugin> }
#[derive(Debug, Serialize)]
pub struct ExtensionMarketplace { pub name: String, pub source: String, pub path: Option<String> }
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionSkill {
    pub name: String, pub description: String, pub path: String, pub scope: String,
    pub enabled: bool, pub plugin_id: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ExtensionError { pub section: &'static str, pub message: String }
#[derive(Debug, Serialize)]
pub struct SkillDestinations { pub user: Option<String>, pub project: Option<String> }
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionsSnapshot {
    pub executable: String, pub cwd: Option<String>,
    pub plugins: Option<ExtensionPlugins>, pub marketplaces: Option<Vec<ExtensionMarketplace>>,
    pub skills: Option<Vec<ExtensionSkill>>, pub skill_destinations: SkillDestinations,
    pub errors: Vec<ExtensionError>,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionMutation {
    InstallPlugin { selector: String },
    RemovePlugin { plugin_id: String },
    SetPluginEnabled { plugin_id: String, enabled: bool },
    AddMarketplace { source: String, git_ref: Option<String> },
    RemoveMarketplace { name: String },
    UpgradeMarketplace { name: Option<String> },
    SetSkillEnabled { path: String, enabled: bool },
    ImportSkill { source_path: String, name: String, scope: SkillImportScope },
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillImportScope { User, Project }

fn admission() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    EXTENSIONS_REQUEST.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => "Another extension operation is in progress; wait for its result before retrying".into(),
        std::sync::TryLockError::Poisoned(_) => "Extension operation lock is damaged; restart OpenDIY Desktop".into(),
    })
}
/// A non-Send permit covers an entire management operation, including its
/// process/file cleanup and final readback. Shutdown uses this same admission
/// boundary, then sets Engine.exiting before releasing it.
pub(crate) struct ExtensionPermit { _request: std::sync::MutexGuard<'static, ()> }
pub(crate) fn extension_admission() -> Result<ExtensionPermit, String> {
    Ok(ExtensionPermit { _request: admission()? })
}
fn context(cwd: Option<&str>) -> Result<PathBuf, String> {
    if let Some(cwd) = cwd { argument(cwd, "Project path", 128 * 1024)?; }
    match cwd {
        Some(path) => Ok(PathBuf::from(crate::project::directory(path)?.identity)),
        None => env::temp_dir().canonicalize().map_err(|e| format!("Cannot open extension process directory: {e}")),
    }
}
fn with_rpc<T>(exe: &OsString, cwd: &Path, operation: impl FnOnce(&mut Rpc<TcpStream>) -> Result<T, String>) -> Result<T, String> {
    let (server, mut rpc) = launch_server(exe, cwd, None)?;
    let result = operation(&mut rpc);
    let _ = rpc.ws.close(None);
    let _ = rpc.ws.get_ref().shutdown(Shutdown::Both);
    drop(rpc);
    drop(server);
    result
}

struct CliProcess {
    child: Child, job: Option<ProcessJob>, _gate: LaunchGate,
    readers: Vec<JoinHandle<Result<Vec<u8>, String>>>,
}
impl Drop for CliProcess {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() { let _ = job.terminate(); }
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) { let _ = reader.join(); }
    }
}
fn capture(mut pipe: impl Read + Send + 'static, overflow: Arc<AtomicBool>) -> JoinHandle<Result<Vec<u8>, String>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut block = [0u8; 16 * 1024];
        loop {
            let count = pipe.read(&mut block).map_err(|e| format!("Read Codex command output: {e}"))?;
            if count == 0 { return Ok(bytes); }
            let remaining = MAX_FRAME.saturating_sub(bytes.len());
            bytes.extend_from_slice(&block[..count.min(remaining)]);
            if count > remaining { overflow.store(true, Ordering::Release); }
        }
    })
}
// JSON 명령과 startup 도움말이 같은 프로세스 소유권·출력 상한을 사용한다.
pub(super) fn command_stdout(exe: &OsString, cwd: &Path, args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    command_stdout_until(exe, cwd, args, Instant::now() + timeout, None)
}
pub(super) fn command_stdout_until(exe: &OsString, cwd: &Path, args: &[String], deadline: Instant, cancelled: Option<&Arc<AtomicBool>>) -> Result<Vec<u8>, String> {
    command_stdout_with_job(exe, cwd, args, deadline, cancelled, false)
}
// 명시적인 서버 시작 명령만 공유 서버가 launcher의 Job 밖에서 살아남도록 허용한다.
// 조회·설치·업데이트·일반 PTY에는 이 정책을 사용할 수 없다.
pub(super) fn start_daemon_stdout_until(exe: &OsString, cwd: &Path, deadline: Instant, cancelled: Option<&Arc<AtomicBool>>) -> Result<Vec<u8>, String> {
    command_stdout_with_job(exe, cwd, &["app-server".into(), "daemon".into(), "start".into(), "--existing-installation".into()], deadline, cancelled, true)
}
fn command_stdout_with_job(exe: &OsString, cwd: &Path, args: &[String], deadline: Instant, cancelled: Option<&Arc<AtomicBool>>, daemon_launcher: bool) -> Result<Vec<u8>, String> {
    check_cancelled(cancelled)?;
    if Instant::now() >= deadline { return Err("Codex command timed out; its result is unknown. Refresh before retrying".into()); }
    let (mut command, gate) = LaunchGate::wrap_std(exe.as_os_str())?;
    // 조회용 CLI에는 TUI의 일회 제어 자격을 상속하지 않는다.
    command.env_remove("CODEX_DESKTOP_CRON_BOOTSTRAP").env_remove("CODEX_DESKTOP_CRON_TOKEN");
    command.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hidden(&mut command);
    let child = command.spawn().map_err(|e| format!("Start Codex command: {e}"))?;
    let mut process = CliProcess { child, job: None, _gate: gate, readers: Vec::new() };
    process.job = Some(if daemon_launcher { ProcessJob::assign_daemon_launcher(&process.child)? } else { ProcessJob::assign_std(&process.child)? });
    let overflow = Arc::new(AtomicBool::new(false));
    process.readers.push(capture(process.child.stdout.take().ok_or("Codex stdout pipe is missing")?, overflow.clone()));
    process.readers.push(capture(process.child.stderr.take().ok_or("Codex stderr pipe is missing")?, overflow.clone()));
    process._gate.release()?;
    let status = loop {
        check_cancelled(cancelled)?;
        if overflow.load(Ordering::Acquire) { return Err("Codex command output exceeded 8 MiB; operation result may be unknown. Refresh before retrying".into()); }
        if let Some(status) = process.child.try_wait().map_err(|e| format!("Inspect Codex command: {e}"))? { break status; }
        if Instant::now() >= deadline { return Err("Codex command timed out; its result is unknown. Refresh before retrying".into()); }
        thread::sleep(Duration::from_millis(20));
    };
    // Stop owned descendants before joining pipes: a grandchild must not keep
    // stdout open after the command exits. No external process is targeted.
    if let Some(job) = process.job.take() { job.terminate()?; }
    // 한 reader의 오류로 나머지 JoinHandle을 버리지 않고 모두 정산한다.
    let joined: Vec<_> = process.readers.drain(..).map(|reader| {
        reader.join().map_err(|_| "Codex command output reader panicked".to_owned()).and_then(|output| output)
    }).collect();
    let mut output = joined.into_iter().collect::<Result<Vec<_>, String>>()?;
    if overflow.load(Ordering::Acquire) { return Err("Codex command output exceeded 8 MiB; refresh before retrying".into()); }
    if !status.success() {
        let detail = String::from_utf8_lossy(&output[1][..output[1].len().min(16 * 1024)]);
        return Err(format!("Codex command exited with {status}: {}", detail.trim()));
    }
    Ok(output.remove(0))
}
fn cli(exe: &OsString, cwd: &Path, args: &[String], timeout: Duration) -> Result<Value, String> {
    let output = command_stdout(exe, cwd, args, timeout)?;
    serde_json::from_slice(&output).map_err(|e| format!("Extension CLI returned invalid JSON; its result may be unknown: {e}"))
}
fn cli_args(exe: &OsString, cwd: &Path, args: &[&str]) -> Result<Value, String> {
    cli(exe, cwd, &args.iter().map(|value| (*value).into()).collect::<Vec<_>>(), Duration::from_secs(30))
}
fn text(value: &Value, key: &str, maximum: usize) -> Result<String, String> {
    value[key].as_str().filter(|s| !s.is_empty() && s.len() <= maximum).map(str::to_owned)
        .ok_or_else(|| format!("Extension response has no bounded {key}"))
}
fn optional_text(value: &Value, key: &str, maximum: usize) -> Result<Option<String>, String> {
    if value[key].is_null() { Ok(None) } else { text(value, key, maximum).map(Some) }
}
fn rows<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    bounded_rows(value, key, MAX_ROWS)
}
fn bounded_rows<'a>(value: &'a Value, key: &str, maximum: usize) -> Result<&'a Vec<Value>, String> {
    let rows = value[key].as_array().ok_or_else(|| format!("Extension response has no {key} array"))?;
    if rows.len() > maximum { return Err(format!("Extension {key} array exceeded {maximum} entries")); }
    Ok(rows)
}
fn source(value: &Value) -> Result<String, String> {
    if !value.is_object() { return Err("Extension source is not an object".into()); }
    let result = value.to_string();
    if result.len() > 16 * 1024 { return Err("Extension source exceeded its display limit".into()); }
    Ok(result)
}
fn parse_plugin(value: &Value) -> Result<ExtensionPlugin, String> {
    let plugin_id = text(value, "pluginId", 512)?;
    plugin_selector(&plugin_id)?;
    Ok(ExtensionPlugin {
        plugin_id, name: text(value, "name", 512)?, marketplace_name: text(value, "marketplaceName", 512)?,
        version: optional_text(value, "version", 1024)?,
        enabled: value["enabled"].as_bool().ok_or("Plugin enabled state is missing")?, source: source(&value["source"])?,
        install_policy: optional_text(value, "installPolicy", 1024)?, auth_policy: optional_text(value, "authPolicy", 1024)?,
    })
}
fn parse_plugins(value: &Value) -> Result<ExtensionPlugins, String> {
    let installed = bounded_rows(value, "installed", MAX_PLUGIN_ROWS)?;
    let available = bounded_rows(value, "available", MAX_PLUGIN_ROWS)?;
    if installed.len() + available.len() > MAX_PLUGIN_ROWS { return Err(format!("Plugin list exceeded {MAX_PLUGIN_ROWS} entries")); }
    Ok(ExtensionPlugins {
        installed: installed.iter().map(parse_plugin).collect::<Result<_, _>>()?,
        available: available.iter().map(parse_plugin).collect::<Result<_, _>>()?,
    })
}
fn parse_marketplaces(value: &Value) -> Result<Vec<ExtensionMarketplace>, String> {
    rows(value, "marketplaces")?.iter().map(|value| Ok(ExtensionMarketplace {
        name: text(value, "name", 512)?, path: Some(text(value, "root", 128 * 1024)?),
        source: if value["marketplaceSource"].is_null() { "CLI-discovered".into() } else { source(&value["marketplaceSource"])? },
    })).collect()
}
fn parse_skills(value: &Value) -> Result<(Vec<ExtensionSkill>, Vec<ExtensionError>), String> {
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    for entry in rows(value, "data")? {
        for error in rows(entry, "errors")? {
            if errors.len() == 128 { return Err("Skill response exceeded 128 diagnostic errors".into()); }
            errors.push(ExtensionError { section: "skills", message: format!("{}: {}", optional_text(error, "path", 128 * 1024)?.unwrap_or_default(), text(error, "message", 16 * 1024)?) });
        }
        for skill in rows(entry, "skills")? {
            if skills.len() == MAX_ROWS { return Err("Skill list exceeded 4096 entries".into()); }
            let path = text(skill, "path", 128 * 1024)?;
            if !Path::new(&path).is_absolute() { return Err("Codex returned a relative skill path".into()); }
            skills.push(ExtensionSkill {
                name: text(skill, "name", 1024)?, description: text(skill, "description", 64 * 1024)?, path,
                scope: text(skill, "scope", 64)?, enabled: skill["enabled"].as_bool().ok_or("Skill enabled state is missing")?,
                plugin_id: optional_text(skill, "pluginId", 512)?,
            });
        }
    }
    Ok((skills, errors))
}
fn user_home() -> Result<PathBuf, String> {
    #[cfg(windows)] let key = "USERPROFILE";
    #[cfg(not(windows))] let key = "HOME";
    let path = env::var_os(key).map(PathBuf::from).ok_or("User home directory is unavailable")?;
    if !path.is_absolute() || !path.is_dir() { return Err("User home must be an existing absolute directory".into()); }
    Ok(path)
}
fn destination(base: &Path) -> PathBuf { base.join(".agents").join("skills") }
fn snapshot(exe: &OsString, directory: &Path, cwd: Option<&str>) -> ExtensionsSnapshot {
    let mut result = ExtensionsSnapshot {
        executable: exe.to_string_lossy().into_owned(), cwd: cwd.map(str::to_owned), plugins: None,
        marketplaces: None, skills: None,
        skill_destinations: SkillDestinations { user: None, project: cwd.map(|_| destination(directory).to_string_lossy().into_owned()) }, errors: Vec::new(),
    };
    match user_home() {
        Ok(home) => result.skill_destinations.user = Some(destination(&home).to_string_lossy().into_owned()),
        Err(message) => result.errors.push(ExtensionError { section: "skills", message }),
    }
    match cli_args(exe, directory, &["plugin", "list", "--available", "--json"]).and_then(|value| parse_plugins(&value)) {
        Ok(plugins) => result.plugins = Some(plugins), Err(message) => result.errors.push(ExtensionError { section: "plugins", message }),
    }
    match cli_args(exe, directory, &["plugin", "marketplace", "list", "--json"]).and_then(|value| parse_marketplaces(&value)) {
        Ok(marketplaces) => result.marketplaces = Some(marketplaces), Err(message) => result.errors.push(ExtensionError { section: "marketplaces", message }),
    }
    match with_rpc(exe, directory, |rpc| parse_skills(&rpc.call("skills/list", json!({"cwds":[directory],"forceReload":true}))?)) {
        Ok((skills, errors)) => { result.skills = Some(skills); result.errors.extend(errors); },
        Err(message) => result.errors.push(ExtensionError { section: "skills", message }),
    }
    result
}
pub(crate) fn extensions_list_with_executable(exe: &OsString, cwd: Option<String>, _permit: &ExtensionPermit) -> Result<ExtensionsSnapshot, String> {
    let directory = context(cwd.as_deref())?;
    Ok(snapshot(exe, &directory, cwd.as_deref()))
}
fn argument(value: &str, label: &str, maximum: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(format!("{label} must contain 1–{maximum} bytes without control characters"))
    } else { Ok(()) }
}
fn plugin_selector(value: &str) -> Result<(), String> {
    argument(value, "Plugin ID", 512)?;
    let Some((plugin, marketplace)) = value.split_once('@') else { return Err("Plugin ID must be the exact name@marketplace returned by Codex".into()); };
    let valid = |part: &str, dots: bool| !part.is_empty() && part.len() <= 256
        && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_') || dots && c == '.')
        && (!dots || !part.starts_with('.') && !part.ends_with('.') && !part.contains(".."));
    if !valid(plugin, true) || !valid(marketplace, false) {
        return Err("Plugin ID must contain a valid plugin name and marketplace name separated by @".into());
    }
    Ok(())
}
fn plugin_key(plugin_id: &str) -> Result<String, String> {
    plugin_selector(plugin_id)?;
    Ok(format!("plugins.{}.enabled", serde_json::to_string(plugin_id).map_err(|e| e.to_string())?))
}
fn set_plugin_enabled(exe: &OsString, directory: &Path, plugin_id: &str, enabled: bool) -> Result<(), String> {
    let key = plugin_key(plugin_id)?;
    let plugins = parse_plugins(&cli_args(exe, directory, &["plugin", "list", "--json"])?)?;
    if !plugins.installed.iter().any(|plugin| plugin.plugin_id == plugin_id) { return Err("The exact plugin is no longer installed; refresh the list".into()); }
    with_rpc(exe, directory, |rpc| {
        let config = rpc.call("config/read", json!({"includeLayers":true,"cwd":directory}))?;
        let layer = rows(&config, "layers")?.iter().find(|layer| layer["name"]["type"] == "user" && layer["name"]["profile"].is_null())
            .ok_or("Codex did not return a writable user config layer; no setting was changed")?;
        let file = text(&layer["name"], "file", 128 * 1024)?;
        let version = text(layer, "version", 4096)?;
        rpc.call("config/value/write", json!({"keyPath":key,"value":enabled,"mergeStrategy":"upsert","filePath":file,"expectedVersion":version}))?;
        let updated = rpc.call("config/read", json!({"cwd":directory}))?;
        let actual = updated["config"]["plugins"][plugin_id]["enabled"].as_bool();
        if actual != Some(enabled) { return Err("The user setting was written, but the effective plugin state differs (project or administrator policy may override it); refresh before retrying".into()); }
        Ok(())
    })
}

fn no_reparse(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(windows)] {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 { return Err(format!("Reparse points are not allowed in skill import paths: {}", ancestor.display())); }
                }
                if metadata.file_type().is_symlink() { return Err(format!("Symbolic links are not allowed in skill import paths: {}", ancestor.display())); }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("Inspect skill import path {}: {error}", ancestor.display())),
        }
    }
    Ok(())
}
fn skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || name.starts_with('.') || name.starts_with('-') || !name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_') {
        return Err("Skill folder name must contain 1–64 lowercase letters, digits, hyphens or underscores".into());
    }
    #[cfg(windows)] {
        let upper = name.to_ascii_uppercase();
        if ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"].contains(&upper.as_str()) { return Err("Skill folder name is a reserved Windows device name".into()); }
    }
    Ok(())
}
fn collect_import(root: &Path, path: &Path, files: &mut Vec<(PathBuf, u64)>, bytes: &mut u64, entries: &mut usize) -> Result<(), String> {
    *entries += 1;
    if *entries > MAX_ROWS { return Err("Skill import exceeds 4096 filesystem entries".into()); }
    no_reparse(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|e| format!("Inspect skill source: {e}"))?;
    if metadata.is_dir() {
        if path.components().count() - root.components().count() > 32 { return Err("Skill folder nesting exceeded 32 levels".into()); }
        for entry in fs::read_dir(path).map_err(|e| format!("Read skill source directory: {e}"))? { collect_import(root, &entry.map_err(|e| e.to_string())?.path(), files, bytes, entries)?; }
    } else if metadata.is_file() {
        *bytes = bytes.checked_add(metadata.len()).ok_or("Skill import byte count overflow")?;
        if *bytes > MAX_IMPORT_BYTES || files.len() == MAX_IMPORT_FILES { return Err("Skill import exceeds 1024 files or 32 MiB".into()); }
        files.push((path.strip_prefix(root).map_err(|_| "Skill import escaped its selected directory")?.to_owned(), metadata.len()));
    } else { return Err("Skill import accepts only regular files and directories".into()); }
    Ok(())
}
fn open_source(path: &Path) -> Result<fs::File, String> {
    no_reparse(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path).map_err(|e| format!("Open skill import source: {e}"))?;
    let metadata = file.metadata().map_err(|e| format!("Inspect open skill source: {e}"))?;
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 { return Err("Skill source changed to a reparse point".into()); }
    }
    if !metadata.is_file() { return Err("Skill source changed to a non-file".into()); }
    Ok(file)
}
#[cfg(windows)]
fn commit_new_directory(stage: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" { fn MoveFileW(existing: *const u16, target: *const u16) -> i32; }
    let stage: Vec<u16> = stage.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // MoveFileW (unlike replace-enabled rename) fails if any destination exists.
    if unsafe { MoveFileW(stage.as_ptr(), target.as_ptr()) } == 0 { return Err(format!("Commit skill without overwriting: {}", io::Error::last_os_error())); }
    Ok(())
}
#[cfg(not(windows))]
fn commit_new_directory(_: &Path, _: &Path) -> Result<(), String> {
    Err("Skill folder import currently requires Windows; no files were registered".into())
}
fn import_skill(source: &str, name: &str, base: &Path) -> Result<PathBuf, String> {
    skill_name(name)?;
    let source = Path::new(source);
    if !source.is_absolute() { return Err("Skill source must be an absolute folder path".into()); }
    no_reparse(source)?;
    no_reparse(base)?;
    let source = source.canonicalize().map_err(|e| format!("Open skill source: {e}"))?;
    let base = base.canonicalize().map_err(|e| format!("Open skill destination base: {e}"))?;
    if !source.is_dir() { return Err("Skill source must be a directory".into()); }
    let target = destination(&base).join(name);
    no_reparse(&target)?;
    if target.starts_with(&source) { return Err("Skill destination cannot be inside the source folder".into()); }
    if fs::symlink_metadata(&target).is_ok() { return Err(format!("Skill destination already exists; no files were overwritten: {}", target.display())); }
    let mut files = Vec::new();
    let mut bytes = 0;
    let mut entries = 0;
    collect_import(&source, &source, &mut files, &mut bytes, &mut entries)?;
    let skill = files.iter().find(|(path, _)| path == Path::new("SKILL.md")).ok_or("Selected folder must contain a top-level SKILL.md")?;
    if skill.1 > 1024 * 1024 { return Err("SKILL.md exceeds 1 MiB".into()); }
    let mut document = String::new();
    open_source(&source.join("SKILL.md"))?.take(1024 * 1024 + 1).read_to_string(&mut document).map_err(|e| format!("Read bounded UTF-8 SKILL.md: {e}"))?;
    if document.len() > 1024 * 1024 { return Err("SKILL.md grew beyond 1 MiB during import".into()); }
    if !document.replace("\r\n", "\n").starts_with("---\n") { return Err("SKILL.md must begin with YAML frontmatter; Codex verifies its complete format after import".into()); }
    let parent = base.join(".agents");
    fs::create_dir_all(&parent).map_err(|e| format!("Create .agents directory: {e}"))?;
    no_reparse(&parent)?;
    let stage = parent.join(format!(".agent-grid-import-{}", Uuid::new_v4()));
    fs::create_dir(&stage).map_err(|e| format!("Create isolated skill staging directory: {e}"))?;
    let copied = (|| {
        for (relative, expected) in files {
            let input = source.join(&relative);
            no_reparse(&input)?;
            let output = stage.join(relative);
            fs::create_dir_all(output.parent().ok_or("Skill import file has no parent")?).map_err(|e| format!("Create skill import subdirectory: {e}"))?;
            let mut input = open_source(&input)?;
            let mut output = fs::OpenOptions::new().write(true).create_new(true).open(output).map_err(|e| format!("Create skill import file: {e}"))?;
            let copied = io::copy(&mut Read::by_ref(&mut input).take(expected + 1), &mut output).map_err(|e| format!("Copy skill import file: {e}"))?;
            if copied != expected { return Err("Skill source changed while copying; import was cancelled".into()); }
            output.sync_all().map_err(|e| format!("Flush skill import file: {e}"))?;
        }
        let root = destination(&base);
        fs::create_dir_all(&root).map_err(|e| format!("Create skill registration directory: {e}"))?;
        no_reparse(&root)?;
        commit_new_directory(&stage, &target)?;
        Ok(target.join("SKILL.md"))
    })();
    if copied.is_err() {
        // Only the fresh UUID staging directory is owned here. Never delete a
        // source, an existing destination, or a path whose ancestry changed.
        if stage.parent() != Some(parent.as_path()) { return Err(format!("{}; staging ownership could not be verified at {}", copied.unwrap_err(), stage.display())); }
        no_reparse(&stage).map_err(|error| format!("{}; staging cleanup skipped: {error}", copied.as_ref().unwrap_err()))?;
        if let Err(error) = fs::remove_dir_all(&stage) { return Err(format!("{}; staging cleanup also failed at {}: {error}", copied.unwrap_err(), stage.display())); }
    }
    copied
}

pub(crate) fn extensions_mutate_with_executable(exe: &OsString, cwd: Option<String>, request: ExtensionMutation, _permit: &ExtensionPermit) -> Result<ExtensionsSnapshot, String> {
    let directory = context(cwd.as_deref())?;
    let mut expected_plugin: Option<(String, Option<bool>)> = None;
    let mut expected_skill: Option<(PathBuf, Option<bool>)> = None;
    let mut expected_marketplace: Option<(String, bool)> = None;
    let mut args: Vec<String> = Vec::new();
    match request {
        ExtensionMutation::InstallPlugin { selector } => {
            plugin_selector(&selector)?;
            expected_plugin = Some((selector.clone(), None));
            args = vec!["plugin".into(), "add".into(), "--json".into(), "--".into(), selector];
        }
        ExtensionMutation::RemovePlugin { plugin_id } => {
            plugin_selector(&plugin_id)?;
            expected_plugin = Some((plugin_id.clone(), Some(false)));
            args = vec!["plugin".into(), "remove".into(), "--json".into(), "--".into(), plugin_id];
        }
        ExtensionMutation::SetPluginEnabled { plugin_id, enabled } => {
            set_plugin_enabled(exe, &directory, &plugin_id, enabled)?;
            expected_plugin = Some((plugin_id, Some(enabled)));
        }
        ExtensionMutation::AddMarketplace { source, git_ref } => {
            argument(&source, "Marketplace source", 4096)?;
            args = vec!["plugin".into(), "marketplace".into(), "add".into(), "--json".into()];
            if let Some(git_ref) = git_ref { argument(&git_ref, "Git ref", 1024)?; args.extend(["--ref".into(), git_ref]); }
            args.extend(["--".into(), source]);
        }
        ExtensionMutation::RemoveMarketplace { name } => {
            argument(&name, "Marketplace name", 512)?;
            expected_marketplace = Some((name.clone(), false));
            args = vec!["plugin".into(), "marketplace".into(), "remove".into(), "--json".into(), "--".into(), name];
        }
        ExtensionMutation::UpgradeMarketplace { name } => {
            args = vec!["plugin".into(), "marketplace".into(), "upgrade".into(), "--json".into()];
            if let Some(name) = name { argument(&name, "Marketplace name", 512)?; args.extend(["--".into(), name]); }
        }
        ExtensionMutation::SetSkillEnabled { path, enabled } => {
            argument(&path, "Skill path", 128 * 1024)?;
            if !Path::new(&path).is_absolute() { return Err("Selected skill path must be absolute".into()); }
            let selected = Path::new(&path).canonicalize().map_err(|e| format!("Open selected skill: {e}"))?;
            let effective = with_rpc(exe, &directory, |rpc| {
                let (skills, _) = parse_skills(&rpc.call("skills/list", json!({"cwds":[directory],"forceReload":true}))?)?;
                if !skills.iter().any(|skill| Path::new(&skill.path).canonicalize().ok().as_ref() == Some(&selected)) { return Err("Selected skill is no longer in the CLI list; no setting was changed".into()); }
                let result = rpc.call("skills/config/write", json!({"path":selected,"enabled":enabled}))?;
                result["effectiveEnabled"].as_bool().ok_or_else(|| "Skill write returned no effective enabled state; refresh before retrying".into())
            })?;
            if effective != enabled { return Err("Skill setting was written, but an effective policy overrides the requested state".into()); }
            expected_skill = Some((selected, Some(enabled)));
        }
        ExtensionMutation::ImportSkill { source_path, name, scope } => {
            let base = match scope {
                SkillImportScope::User => user_home()?,
                SkillImportScope::Project if cwd.is_some() => directory.clone(),
                SkillImportScope::Project => return Err("Select a project before importing a project skill".into()),
            };
            expected_skill = Some((import_skill(&source_path, &name, &base)?, None));
        }
    }
    if !args.is_empty() {
        let response = cli(exe, &directory, &args, Duration::from_secs(120))?;
        if args.get(2).map(String::as_str) == Some("add") && args.get(1).map(String::as_str) == Some("marketplace") {
            expected_marketplace = Some((text(&response, "marketplaceName", 512)?, true));
        }
    }
    let result = snapshot(exe, &directory, cwd.as_deref());
    if let Some((id, state)) = expected_plugin {
        let plugins = result.plugins.as_ref().ok_or("The CLI operation completed, but plugin verification failed; refresh before retrying")?;
        let found = plugins.installed.iter().find(|plugin| plugin.plugin_id == id);
        let removing = args.get(1).map(String::as_str) == Some("remove");
        if removing && found.is_some() || !removing && found.is_none() || !removing && state.is_some_and(|state| found.is_some_and(|plugin| plugin.enabled != state)) {
            return Err("The CLI operation completed, but the exact plugin state was not verified; refresh before retrying".into());
        }
    }
    if let Some((path, enabled)) = expected_skill {
        let expected = path.canonicalize().map_err(|e| format!("Skill files/settings were saved, but the registered path is no longer readable at {}: {e}", path.display()))?;
        let found = result.skills.as_ref().and_then(|skills| skills.iter().find(|skill| Path::new(&skill.path).canonicalize().ok().as_ref() == Some(&expected)));
        if found.is_none() || enabled.is_some_and(|enabled| found.is_some_and(|skill| skill.enabled != enabled)) {
            return Err(format!("Skill files/settings were saved, but Codex did not verify the requested skill state at {}. Inspect skill errors and refresh; no existing files were overwritten", path.display()));
        }
    }
    if let Some((name, present)) = expected_marketplace {
        let marketplaces = result.marketplaces.as_ref().ok_or("The CLI operation completed, but marketplace verification failed; refresh before retrying")?;
        if marketplaces.iter().any(|marketplace| marketplace.name == name) != present { return Err("The CLI operation completed, but marketplace state was not verified; refresh before retrying".into()); }
    }
    if args.get(1).map(String::as_str) == Some("marketplace") && result.marketplaces.is_none() { return Err("Marketplace CLI operation completed, but the refreshed list failed; refresh before retrying".into()); }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_plugin_keys_do_not_split_dotted_names_or_accept_options() {
        assert_eq!(plugin_key("same.name@market-place").unwrap(), "plugins.\"same.name@market-place\".enabled");
        for id in ["--config", "x@y@z", "x@", "x@market\n", "../x@y", "x\"@y", "a@market.place", ".a@b", "a..x@b"] { assert!(plugin_key(id).is_err(), "{id}"); }
        let request: ExtensionMutation = serde_json::from_value(json!({"operation":"setPluginEnabled","pluginId":"a@b","enabled":false})).unwrap();
        assert!(matches!(request, ExtensionMutation::SetPluginEnabled { enabled: false, .. }));
        assert!(serde_json::from_value::<ExtensionMutation>(json!({"operation":"arbitraryRpc","method":"turn/start"})).is_err());
    }
    #[test]
    fn malformed_or_partial_lists_are_errors_not_empty_successes() {
        assert!(parse_plugins(&json!({})).is_err());
        assert!(parse_marketplaces(&json!({"marketplaces":null})).is_err());
        let plugin = json!({"pluginId":"name@one","name":"name","marketplaceName":"one","version":null,"enabled":true,"source":{"source":"remote","id":"p"},"installPolicy":"AVAILABLE","authPolicy":"ON_USE"});
        let plugins = parse_plugins(&json!({"installed":[plugin],"available":[]})).unwrap();
        assert_eq!(plugins.installed[0].plugin_id, "name@one");
        assert!(parse_skills(&json!({"data":[{"skills":[],"errors":[{"path":"broken","message":"bad YAML"}]}]})).unwrap().1.len() == 1);
    }
    #[test]
    fn plugin_catalog_accepts_observed_size_but_preserves_shape_and_combined_bounds() {
        let plugin = json!({"pluginId":"name@one","name":"name","marketplaceName":"one","version":null,"enabled":true,"source":{"source":"remote","id":"fixture"}});
        let catalog = json!({"installed":vec![plugin.clone();20],"available":vec![plugin.clone();9161]});
        let parsed = parse_plugins(&catalog).unwrap();
        assert_eq!(parsed.installed.len(), 20);
        assert_eq!(parsed.available.len(), 9161);
        assert!(parse_plugins(&json!({"installed":[],"available":vec![plugin.clone();16384]})).is_ok());
        let error = parse_plugins(&json!({"installed":[plugin.clone()],"available":vec![plugin.clone();16384]})).err().unwrap();
        assert!(error.contains("16384"), "{error}");
        let error = parse_plugins(&json!({"installed":[],"available":vec![plugin.clone();16385]})).err().unwrap();
        assert!(error.contains("16384"), "{error}");
        for invalid in [json!({}), json!({"installed":[]}), json!({"available":[]}), json!({"installed":[],"available":null}), json!({"installed":[],"available":{}}), json!({"installed":[],"available":[{}]})] {
            assert!(parse_plugins(&invalid).is_err());
        }
        assert!(rows(&json!({"data":vec![Value::Null;4097]}), "data").is_err());
        assert!(parse_marketplaces(&json!({"marketplaces":vec![Value::Null;4097]})).is_err());
    }
    #[test]
    #[ignore = "선택한 실제 CLI의 조회 명령만 명시적으로 실행한다"]
    fn selected_cli_plugin_catalog_is_parsed_without_configuration_changes() {
        let exe = env::var_os("AGENT_GRID_EXTENSION_READ_ONLY_EXE").expect("조회할 CLI 실행 파일 환경 변수가 필요하다");
        let cwd = PathBuf::from(env::var_os("AGENT_GRID_EXTENSION_READ_ONLY_CWD").expect("조회할 실행 폴더 환경 변수가 필요하다"));
        let Ok(value) = cli_args(&exe, &cwd, &["plugin", "list", "--available", "--json"]) else { panic!("실제 플러그인 조회 실패: 원문 오류는 비밀 보존을 위해 출력하지 않는다"); };
        let parsed = parse_plugins(&value);
        assert!(parsed.is_ok(), "실제 플러그인 응답 해석 실패: 원문 값은 출력하지 않는다");
        let parsed = parsed.unwrap();
        println!("실제 조회 응답 해석 통과: 설치됨={}, 설치 가능={}", parsed.installed.len(), parsed.available.len());
    }
    #[test]
    fn output_capture_is_bounded() {
        let overflow = Arc::new(AtomicBool::new(false));
        let output = capture(io::Cursor::new(vec![b'x'; MAX_FRAME + 100]), overflow.clone()).join().unwrap().unwrap();
        assert_eq!(output.len(), MAX_FRAME);
        assert!(overflow.load(Ordering::Acquire));
    }
    #[test]
    #[cfg(windows)]
    fn owned_cli_runner_preserves_argv_and_drains_both_pipes() {
        let root = env::temp_dir().join(format!("agent-grid-cli-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let script = root.join("fixture.ps1");
        fs::write(&script, r#"param([string]$Mode,[string]$Value,[string]$StageFile)
[IO.File]::WriteAllText($StageFile,$Mode + ':entered')
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
switch ($Mode) {
 'argv' { [Console]::Out.Write($Value) }
 'pipes' { [Console]::Error.Write(('diagnostic ' * 65536)); [Console]::Out.Write('{"ok":true}') }
 'nonzero' { [Console]::Error.Write('fixture failure'); exit 23 }
 'invalid' { [Console]::Out.Write('{partial') }
}
[IO.File]::WriteAllText($StageFile,$Mode + ':complete')
"#).unwrap();
        let exe = OsString::from(env::var_os("SystemRoot").map(PathBuf::from).unwrap().join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        let stage = root.join("stage.txt");
        let arguments = |mode: &str, value: &str| vec!["-NoProfile".into(), "-NonInteractive".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-File".into(), script.to_string_lossy().into_owned(), "-Mode".into(), mode.into(), "-Value".into(), value.into(), "-StageFile".into(), stage.to_string_lossy().into_owned()];
        // 정상 명령은 실제 조회와 같은 30초 한도를 쓴다. 아래 별도 시험이
        // 짧은 timeout과 자손의 파이프 보유를 직접 검증한다.
        let run = |mode: &str, value: &str| {
            if stage.exists() { fs::remove_file(&stage).unwrap(); }
            let started = Instant::now();
            let result = command_stdout(&exe, &root, &arguments(mode, value), Duration::from_secs(30));
            let observed = fs::read_to_string(&stage).unwrap_or_else(|_| "script-not-entered".into());
            println!("정상 CLI 시험: mode={mode}, elapsed={:?}, phase={observed}, succeeded={}", started.elapsed(), result.is_ok());
            (result, observed)
        };
        let value = "한글 space; & $(not-executed) \"quote\" @market";
        let (argv, phase) = run("argv", value);
        assert_eq!(String::from_utf8(argv.unwrap_or_else(|error| panic!("argv: {error}; phase={phase}"))).unwrap(), value);
        assert_eq!(phase, "argv:complete");
        let (pipes, phase) = run("pipes", "");
        assert_eq!(serde_json::from_slice::<Value>(&pipes.unwrap_or_else(|error| panic!("pipes: {error}; phase={phase}"))).unwrap()["ok"], true);
        assert_eq!(phase, "pipes:complete");
        let nonzero = run("nonzero", "").0.unwrap_err();
        assert!(nonzero.contains("23") && nonzero.contains("fixture failure"), "{nonzero}");
        assert!(serde_json::from_slice::<Value>(&run("invalid", "").0.unwrap()).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(windows)]
    #[ignore = "소유 CLI 자식 시험에서만 명시적으로 실행하는 순수 Rust fixture"]
    fn owned_cli_runner_fixture_child() {
        let root = env::current_dir().unwrap();
        let mode = fs::read_to_string(root.join("mode.txt")).unwrap();
        fs::write(root.join("entered.txt"), std::process::id().to_string()).unwrap();
        if mode == "flood" {
            for _ in 0..20 { io::stdout().write_all(&vec![b'x';1024*1024]).unwrap(); }
        } else if mode == "timeout" || mode == "pipe-leak" {
            let child_root=root.join("child"); fs::create_dir(&child_root).unwrap();
            fs::write(child_root.join("mode.txt"), "sleep").unwrap();
            let mut command=Command::new(env::current_exe().unwrap());
            command.args(["--exact","codex::extensions::tests::owned_cli_runner_fixture_child","--ignored","--nocapture"]).current_dir(&child_root).stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
            hidden(&mut command); let child=command.spawn().unwrap();
            let deadline=Instant::now()+Duration::from_secs(30);
            while !child_root.join("entered.txt").exists() { assert!(Instant::now()<deadline,"자손 fixture 시작 실패"); thread::sleep(Duration::from_millis(2)); }
            fs::write(root.join("pids.tmp"), format!("{},{}",std::process::id(),child.id())).unwrap();
            fs::rename(root.join("pids.tmp"),root.join("pids.txt")).unwrap();
            while !root.join("release.txt").exists() { assert!(Instant::now()<deadline,"부모 시험의 핸들 관측 실패"); thread::sleep(Duration::from_millis(2)); }
            if mode == "pipe-leak" { std::process::exit(0); }
        }
        thread::sleep(Duration::from_secs(120));
        std::process::exit(0);
    }
    #[test]
    #[cfg(windows)]
    fn owned_cli_runner_enforces_overflow_timeout_and_descendant_pipe_cleanup() {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::{OpenProcess, WaitForSingleObject}};
        struct Probe(*mut std::ffi::c_void);
        impl Drop for Probe { fn drop(&mut self) { unsafe { CloseHandle(self.0); } } }
        let root=env::temp_dir().join(format!("agent-grid-cli-lifetime-{}",Uuid::new_v4())); fs::create_dir(&root).unwrap();
        let exe=env::current_exe().unwrap().into_os_string();
        let args=vec!["--exact".into(),"codex::extensions::tests::owned_cli_runner_fixture_child".into(),"--ignored".into(),"--nocapture".into()];
        fs::write(root.join("mode.txt"),"flood").unwrap();
        assert!(command_stdout(&exe,&root,&args,Duration::from_secs(6)).unwrap_err().contains("exceeded"));
        for mode in ["timeout","pipe-leak"] {
            let directory=root.join(mode); fs::create_dir(&directory).unwrap(); fs::write(directory.join("mode.txt"),mode).unwrap();
            let (send,receive)=std::sync::mpsc::channel();
            let (worker_exe,worker_root,worker_args)=(exe.clone(),directory.clone(),args.clone());
            let started=Instant::now();
            let worker=thread::spawn(move || { let result=command_stdout(&worker_exe,&worker_root,&worker_args,Duration::from_secs(6)); let _=send.send(result); });
            let deadline=Instant::now()+Duration::from_secs(10);
            let pids=directory.join("pids.txt");
            while !pids.exists() {
                if let Ok(result)=receive.try_recv() { panic!("{mode} fixture 준비 전 종료: {result:?}"); }
                assert!(Instant::now()<deadline,"{mode} fixture가 준비되지 않음"); thread::sleep(Duration::from_millis(2));
            }
            // 완료 후 PID를 다시 조회하지 않고, 살아 있는 동안 연 원래 핸들로 확인한다.
            let processes=fs::read_to_string(pids).unwrap().split(',').map(|pid| {
                let pid=pid.parse::<u32>().unwrap();
                let process=unsafe { OpenProcess(0x00100000,0,pid) }; assert!(!process.is_null(),"fixture {pid} 핸들 없음");
                (pid,Probe(process))
            }).collect::<Vec<_>>();
            for (pid,process) in &processes { assert_eq!(unsafe { WaitForSingleObject(process.0,0) },258,"소유 fixture {pid}가 관측 전에 종료됨"); }
            fs::write(directory.join("release.txt"),"observed").unwrap();
            let result=receive.recv_timeout(Duration::from_secs(15)).expect("자손의 pipe가 명령 완료를 막음"); worker.join().unwrap();
            if mode=="timeout" { assert!(result.unwrap_err().contains("timed out")); } else { result.unwrap(); }
            for (pid,process) in processes {
                let status=unsafe { WaitForSingleObject(process.0,1000) };
                assert_eq!(status,0,"소유 fixture {pid}가 종료되지 않음");
            }
            println!("CLI 자원 수명 시험: mode={mode}, ready=true, originalHandlesExited=true, elapsed={:?}",started.elapsed());
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(windows)]
    fn skill_import_preserves_existing_files_and_copies_assets_into_new_directory() {
        let root = env::temp_dir().join(format!("agent-grid-import-test-{}", Uuid::new_v4()));
        let source = root.join("source"); let project = root.join("project");
        fs::create_dir_all(source.join("scripts")).unwrap(); fs::create_dir(&project).unwrap();
        fs::write(source.join("SKILL.md"), "---\nname: sample\ndescription: sample\n---\nInstructions\n").unwrap();
        fs::write(source.join("scripts").join("sample.txt"), "한글 asset").unwrap();
        let selected = import_skill(source.to_str().unwrap(), "sample", &project).unwrap();
        assert_eq!(fs::read_to_string(selected.parent().unwrap().join("scripts/sample.txt")).unwrap(), "한글 asset");
        fs::write(&selected, "user edited").unwrap();
        assert!(import_skill(source.to_str().unwrap(), "sample", &project).is_err());
        assert_eq!(fs::read_to_string(selected).unwrap(), "user edited");
        assert!(import_skill(source.to_str().unwrap(), "../escape", &project).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn skill_import_rejects_missing_manifest_and_oversize_files_before_creating_destination() {
        let root = env::temp_dir().join(format!("agent-grid-import-limit-test-{}", Uuid::new_v4()));
        let source = root.join("source"); let project = root.join("project");
        fs::create_dir_all(&source).unwrap(); fs::create_dir(&project).unwrap();
        assert!(import_skill(source.to_str().unwrap(), "missing", &project).is_err());
        fs::write(source.join("SKILL.md"), "---\nname: sample\ndescription: sample\n---\n").unwrap();
        fs::File::create(source.join("large")).unwrap().set_len(MAX_IMPORT_BYTES + 1).unwrap();
        assert!(import_skill(source.to_str().unwrap(), "large", &project).is_err());
        assert!(!destination(&project).exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(windows)]
    fn skill_import_rejects_junctions_and_atomic_commit_never_replaces_a_destination() {
        let root = env::temp_dir().join(format!("agent-grid-import-link-test-{}", Uuid::new_v4()));
        let source = root.join("source"); let project = root.join("project");
        fs::create_dir_all(&source).unwrap(); fs::create_dir(&project).unwrap();
        fs::write(source.join("SKILL.md"), "---\nname: sample\ndescription: sample\n---\n").unwrap();
        let link = root.join("junction");
        let status = Command::new("cmd.exe").args(["/D", "/C", "mklink", "/J"])
            .arg(&link).arg(&source).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(status.success(), "Create owned junction fixture");
        assert!(import_skill(link.to_str().unwrap(), "sample", &project).unwrap_err().contains("Reparse"));
        assert!(import_skill(source.to_str().unwrap(), "sample", &link).unwrap_err().contains("Reparse"));
        fs::remove_dir(&link).unwrap();
        assert!(source.join("SKILL.md").is_file());
        let stage = root.join("stage"); let existing = root.join("existing");
        fs::create_dir(&stage).unwrap(); fs::create_dir(&existing).unwrap();
        fs::write(stage.join("content"), "owned stage").unwrap();
        assert!(commit_new_directory(&stage, &existing).is_err());
        assert!(stage.join("content").is_file());
        assert_eq!(fs::read_dir(&existing).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
