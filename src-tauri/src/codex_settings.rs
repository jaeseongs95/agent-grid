//! App-owned executable selection. No environment or vendor configuration writes.
use serde::Serialize;
use std::{ffi::{OsStr, OsString}, fs::File, io::{Read, Seek, SeekFrom}, path::{Path, PathBuf}};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSettings {
    pub saved_path: Option<String>,
    pub resolved_path: Option<String>,
    pub source: String,
    pub error: Option<String>,
}

impl CodexSettings {
    /// A request keeps this immutable path for its complete server/TUI pair.
    /// Saving another setting cannot redirect an already admitted request.
    pub fn executable(&self) -> Result<OsString, String> {
        self.resolved_path.as_ref().map(OsString::from)
            .ok_or_else(|| self.error.clone().unwrap_or_else(|| "Codex 실행 파일을 선택하세요.".into()))
    }
}

fn display_path(path: &Path) -> Result<String, String> {
    let value = path.to_str().ok_or("실행 파일 경로를 UTF-8 문자열로 표현할 수 없습니다.")?;
    #[cfg(windows)] {
        if let Some(value) = value.strip_prefix("\\\\?\\UNC\\") { return Ok(format!("\\\\{value}")); }
        if let Some(value) = value.strip_prefix("\\\\?\\") { return Ok(value.to_owned()); }
    }
    Ok(value.to_owned())
}

pub fn validate(path: &Path) -> Result<String, String> {
    if !path.is_absolute() { return Err("Codex 실행 파일은 절대 경로여야 합니다.".into()); }
    let is_exe = |path: &Path| path.extension().and_then(OsStr::to_str).is_some_and(|value| value.eq_ignore_ascii_case("exe"));
    if !is_exe(path) {
        return Err("네이티브 Windows .exe 파일을 선택하세요. .cmd·.bat·.ps1 실행기는 지원하지 않습니다.".into());
    }
    let canonical = path.canonicalize().map_err(|e| format!("Codex 실행 파일 경로를 열 수 없습니다: {e}"))?;
    if !is_exe(&canonical) { return Err("실제 실행 파일도 .exe 확장자여야 합니다. 셸 실행기는 지원하지 않습니다.".into()); }
    let mut file = File::open(&canonical).map_err(|e| format!("Codex 실행 파일을 읽을 수 없습니다: {e}"))?;
    let metadata = file.metadata().map_err(|e| format!("Codex 실행 파일 정보를 읽을 수 없습니다: {e}"))?;
    if !metadata.is_file() { return Err("Codex 실행 파일 경로는 일반 파일이어야 합니다.".into()); }
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos).map_err(|_| "선택한 파일은 Windows 실행 파일 헤더가 없습니다.".to_owned())?;
    if &dos[..2] != b"MZ" { return Err("선택한 파일은 네이티브 Windows 실행 파일이 아닙니다.".into()); }
    let offset = u32::from_le_bytes(dos[60..64].try_into().expect("four byte header field")) as u64;
    if offset < 64 || offset.checked_add(24).is_none_or(|end| end > metadata.len()) {
        return Err("선택한 파일의 Windows 실행 파일 헤더가 올바르지 않습니다.".into());
    }
    file.seek(SeekFrom::Start(offset)).map_err(|e| format!("Windows 실행 파일 헤더를 읽을 수 없습니다: {e}"))?;
    let mut pe = [0u8; 24];
    file.read_exact(&mut pe).map_err(|e| format!("Windows 실행 파일 헤더를 읽을 수 없습니다: {e}"))?;
    let characteristics = u16::from_le_bytes([pe[22], pe[23]]);
    if &pe[..4] != b"PE\0\0" || characteristics & 0x0002 == 0 || characteristics & 0x2000 != 0 {
        return Err("선택한 파일은 Windows .exe 프로그램이 아닙니다.".into());
    }
    display_path(&canonical)
}

fn status(saved: Option<&str>, candidate: &Path, source: &str) -> CodexSettings {
    let (resolved_path, error) = match validate(candidate) {
        Ok(path) => (Some(path), None),
        Err(error) => (None, Some(error)),
    };
    CodexSettings { saved_path: saved.map(str::to_owned), resolved_path, source: source.into(), error }
}

fn resolve_with(saved: Option<&str>, environment: Option<&OsStr>, search_path: Option<&OsStr>) -> CodexSettings {
    if let Some(path) = saved { return status(saved, Path::new(path), "saved"); }
    if let Some(path) = environment { return status(None, Path::new(path), "environment"); }
    for directory in std::env::split_paths(search_path.unwrap_or_else(|| OsStr::new(""))) {
        let path = directory.join("codex.exe");
        if path.is_file() {
            // PATH may contain a relative directory. Its resolution is captured
            // now, while manually saved/environment paths must be absolute.
            let absolute = if path.is_absolute() { path } else {
                match std::env::current_dir() {
                    Ok(directory) => directory.join(path),
                    Err(error) => return CodexSettings { saved_path: None, resolved_path: None, source: "path".into(), error: Some(format!("PATH의 상대 경로를 해석할 수 없습니다: {error}")) },
                }
            };
            return status(None, &absolute, "path");
        }
    }
    CodexSettings { saved_path: None, resolved_path: None, source: "unavailable".into(), error: Some("Codex 실행 파일을 찾지 못했습니다. 앱 설정에서 네이티브 Codex .exe 파일을 선택하세요.".into()) }
}

pub fn resolve(saved: Option<&str>) -> CodexSettings {
    resolve_with(saved, std::env::var_os("AGENT_GRID_CODEX").as_deref(), std::env::var_os("PATH").as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, String) {
        let directory = std::env::temp_dir().join(format!("agent-grid-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("codex.exe");
        let mut header = [0u8; 128];
        header[..2].copy_from_slice(b"MZ");
        header[60..64].copy_from_slice(&64u32.to_le_bytes());
        header[64..68].copy_from_slice(b"PE\0\0");
        header[86..88].copy_from_slice(&0x0002u16.to_le_bytes());
        std::fs::write(&path, header).unwrap();
        let canonical = validate(&path).unwrap();
        (directory, canonical)
    }
    #[test]
    fn resolver_priority_and_snapshot_do_not_require_environment_mutation_or_execution() {
        let (directory, path) = fixture();
        let invalid = directory.join("missing.exe").to_string_lossy().into_owned();
        let selected = resolve_with(Some(&path), Some(OsStr::new(&invalid)), None);
        let snapshot = selected.executable().unwrap();
        let old_command = portable_pty::CommandBuilder::new(&snapshot);
        assert_eq!(selected.source, "saved");
        assert_eq!(selected.resolved_path.as_deref(), Some(path.as_str()));
        let changed = resolve_with(Some(&invalid), Some(OsStr::new(&path)), None);
        assert_eq!(changed.source, "saved");
        assert!(changed.resolved_path.is_none());
        assert!(changed.error.is_some());
        assert_eq!(snapshot, OsString::from(&path));
        let second = directory.join("second.exe");
        std::fs::copy(&path, &second).unwrap();
        let second = validate(&second).unwrap();
        let replacement = resolve_with(Some(&second), None, None).executable().unwrap();
        assert_ne!(replacement, snapshot);
        assert_eq!(old_command.get_argv()[0], snapshot);
        assert_eq!(portable_pty::CommandBuilder::new(&replacement).get_argv()[0], replacement);
        let environment = resolve_with(None, Some(OsStr::new(&path)), None);
        assert_eq!(environment.source, "environment");
        let search = std::env::join_paths([directory.as_path()]).unwrap();
        assert_eq!(resolve_with(None, None, Some(&search)).source, "path");
        assert_eq!(resolve_with(None, None, None).source, "unavailable");
        std::fs::remove_file(directory.join("codex.exe")).unwrap();
        std::fs::remove_file(&second).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn validation_rejects_relative_paths_shims_and_disguised_text_without_running_them() {
        assert!(validate(Path::new("codex.exe")).is_err());
        let (directory, path) = fixture();
        let shim = directory.join("codex.cmd");
        std::fs::copy(&path, &shim).unwrap();
        assert!(validate(&shim).is_err());
        std::fs::write(&path, b"not an executable").unwrap();
        assert!(validate(Path::new(&path)).is_err());
        std::fs::remove_file(shim).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
