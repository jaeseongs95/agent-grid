//! App-owned project metadata and the Windows folder chooser.
use serde::Serialize;
use std::{path::Path, sync::{atomic::{AtomicBool, Ordering}, Arc}};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub cwd: Option<String>,
    pub created_at: i64,
    pub favorite: bool,
}

pub const MAX_PROJECTS: usize = 250;

pub fn next_revision(revision: u64) -> Result<u64, String> {
    // 브라우저에서도 정수 비교가 정확한 범위만 사용한다.
    revision.checked_add(1).filter(|next| *next <= 9_007_199_254_740_991).ok_or_else(|| "Project assignment revision limit reached".into())
}

pub fn validate_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err("Project name must contain 1..120 characters without control characters".into());
    }
    Ok(name)
}

pub struct ProjectDirectory {
    pub identity: String,
    pub cwd: String,
    pub name: String,
}

pub fn directory(cwd: &str) -> Result<ProjectDirectory, String> {
    let path = Path::new(cwd);
    if !path.is_absolute() { return Err("Project directory must be an absolute path".into()); }
    let canonical = path.canonicalize().map_err(|e| format!("Cannot open project directory: {e}"))?;
    if !canonical.is_dir() { return Err("Project path must be a directory".into()); }
    let identity = canonical.to_str().ok_or("Project directory is not valid Unicode")?.to_owned();
    // Canonical identity preserves case-sensitive directory names. Only remove
    // Windows' extended-length prefix from the path presented to the user.
    let cwd = display_path(&identity);
    let name = canonical.file_name().and_then(|name| name.to_str()).filter(|name| !name.is_empty()).unwrap_or(&cwd).to_owned();
    Ok(ProjectDirectory { identity, cwd, name })
}

fn display_path(path: &str) -> String {
    #[cfg(windows)] {
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") { return format!(r"\\{unc}"); }
        if let Some(local) = path.strip_prefix(r"\\?\") { return local.to_owned(); }
    }
    path.to_owned()
}

pub fn same_cwd(left: &str, right: &str) -> bool {
    fn normalized(path: &str) -> String {
        let path = display_path(path).replace('\\', "/");
        let path = path.trim_end_matches('/');
        #[cfg(windows)] { path.to_lowercase() }
        #[cfg(not(windows))] { path.to_owned() }
    }
    normalized(left) == normalized(right)
}

#[derive(Clone, Default)]
pub struct FolderPicker { busy: Arc<AtomicBool> }
struct BusyPicker(Arc<AtomicBool>);
impl Drop for BusyPicker {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

impl FolderPicker {
    pub fn choose(&self, owner: isize, language: crate::ui_language::UiLanguage) -> Result<Option<String>, String> {
        self.choose_purpose(owner, language, false)
    }

    pub fn choose_skill(&self, owner: isize, language: crate::ui_language::UiLanguage) -> Result<Option<String>, String> {
        self.choose_purpose(owner, language, true)
    }

    fn choose_purpose(&self, owner: isize, language: crate::ui_language::UiLanguage, skill: bool) -> Result<Option<String>, String> {
        self.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "The project folder chooser is already open")?;
        let _busy = BusyPicker(self.busy.clone());
        // The async runtime's blocking pool may have an existing MTA. Shell's
        // new-style folder chooser requires a fresh STA, never a changed mode.
        std::thread::Builder::new().name("project-folder-picker".into())
            .spawn(move || choose_folder(owner, language, skill)).map_err(|e| format!("Start folder chooser: {e}"))?
            .join().map_err(|_| "Project folder chooser worker panicked".to_owned())?
    }
}

#[cfg(windows)]
fn choose_folder(owner: isize, language: crate::ui_language::UiLanguage, skill: bool) -> Result<Option<String>, String> {
    use windows::{core::{HRESULT, PCWSTR, PWSTR}, Win32::{
        Foundation::{ERROR_CANCELLED, HWND},
        System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
        UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, SIGDN_FILESYSPATH},
    }};
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self) { unsafe { CoUninitialize(); } } }
    struct FolderPath(PWSTR);
    impl Drop for FolderPath { fn drop(&mut self) { unsafe { CoTaskMemFree(Some(self.0.0.cast())); } } }
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok()
        .map_err(|e| format!("Initialize folder chooser COM apartment: {e}"))?;
    // 취소·오류 경로도 인터페이스와 경로 메모리를 먼저 해제하도록
    // COM 아파트먼트 수명 보호자를 먼저 선언한다.
    let _apartment = Apartment;
    let dialog: IFileOpenDialog = unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("Create folder chooser: {e}"))?;
    let options = unsafe { dialog.GetOptions() }.map_err(|e| format!("Read folder chooser options: {e}"))?;
    unsafe { dialog.SetOptions((options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR) & !FOS_ALLOWMULTISELECT) }
        .map_err(|e| format!("Set folder chooser options: {e}"))?;
    let title: Vec<u16> = (if skill { language.text("SKILL.md가 있는 스킬 폴더를 선택하세요", "Select a skill folder containing SKILL.md") } else { language.text("프로젝트로 가져올 폴더를 선택하세요", "Select a folder to import as a project") }).encode_utf16().chain(Some(0)).collect();
    unsafe { dialog.SetTitle(PCWSTR(title.as_ptr())) }.map_err(|e| format!("Set folder chooser title: {e}"))?;
    if let Err(e) = unsafe { dialog.Show(Some(HWND(owner as *mut std::ffi::c_void))) } {
        if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) { return Ok(None); }
        return Err(format!("Show folder chooser: {e}"));
    }
    let selected = unsafe { dialog.GetResult() }.map_err(|e| format!("Read selected folder: {e}"))?;
    let path = FolderPath(unsafe { selected.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(|e| format!("Read selected folder filesystem path: {e}"))?);
    if path.0.is_null() { return Err("Folder chooser returned an empty filesystem path".into()); }
    let path = unsafe { path.0.to_string() }.map_err(|_| "Selected project path is not valid Unicode".to_owned())?;
    let metadata = std::fs::metadata(&path).map_err(|e| format!("Read selected folder metadata: {e}"))?;
    if !metadata.is_dir() { return Err("Selected path must be an existing directory".into()); }
    Ok(Some(path))
}

#[cfg(not(windows))]
fn choose_folder(_: isize, _: crate::ui_language::UiLanguage, _: bool) -> Result<Option<String>, String> {
    Err("The native project folder chooser is supported only on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_directory_requires_an_existing_absolute_directory() {
        assert!(directory("relative-project").is_err());
        let missing = std::env::temp_dir().join(format!("agent-grid-missing-{}", uuid::Uuid::new_v4()));
        assert!(directory(missing.to_str().unwrap()).is_err());
        let source_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(directory(source_file.to_str().unwrap()).is_err());
    }
}
