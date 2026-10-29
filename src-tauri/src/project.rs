//! App-owned project metadata and the Windows folder chooser.
use serde::Serialize;
use std::{path::Path, sync::{atomic::{AtomicBool, Ordering}, Arc}};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub created_at: i64,
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
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::{
        Foundation::HWND,
        System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
        UI::Shell::{Common::ITEMIDLIST, SHBrowseForFolderW, SHGetPathFromIDListEx, BROWSEINFOW, BIF_EDITBOX, BIF_NEWDIALOGSTYLE, BIF_NONEWFOLDERBUTTON, BIF_RETURNONLYFSDIRS, GPFIDL_DEFAULT},
    };
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self) { unsafe { CoUninitialize(); } } }
    struct FolderId(*mut ITEMIDLIST);
    impl Drop for FolderId { fn drop(&mut self) { unsafe { CoTaskMemFree(self.0.cast()); } } }
    let initialized = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
    if initialized < 0 { return Err(format!("Initialize folder chooser COM apartment failed: {initialized:#010x}")); }
    let _apartment = Apartment;
    let title: Vec<u16> = (if skill { language.text("SKILL.md가 있는 스킬 폴더를 선택하세요", "Select a skill folder containing SKILL.md") } else { language.text("프로젝트로 가져올 폴더를 선택하세요", "Select a folder to import as a project") }).encode_utf16().chain(Some(0)).collect();
    let mut display_name = vec![0u16; 32768];
    let browse = BROWSEINFOW {
        hwndOwner: owner as HWND,
        pszDisplayName: display_name.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX | BIF_NONEWFOLDERBUTTON,
        ..Default::default()
    };
    let selected = unsafe { SHBrowseForFolderW(&browse) };
    if selected.is_null() { return Ok(None); }
    let selected = FolderId(selected);
    let mut path = vec![0u16; 32768];
    if unsafe { SHGetPathFromIDListEx(selected.0, path.as_mut_ptr(), path.len() as u32, GPFIDL_DEFAULT) } == 0 {
        return Err("Selected folder has no readable filesystem path".into());
    }
    let end = path.iter().position(|unit| *unit == 0).ok_or("Folder chooser returned an unterminated path")?;
    OsString::from_wide(&path[..end]).into_string().map(Some).map_err(|_| "Selected project path is not valid Unicode".into())
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
