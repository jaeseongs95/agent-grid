//! Native executable selection only; validation and persistence belong to settings.
use std::{path::Path, sync::{atomic::{AtomicBool, Ordering}, Arc}};

#[derive(Clone, Default)]
pub struct ExecutablePicker { busy: Arc<AtomicBool> }
struct BusyPicker(Arc<AtomicBool>);
impl Drop for BusyPicker {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

impl ExecutablePicker {
    pub fn choose(&self, owner: isize, language: crate::ui_language::UiLanguage) -> Result<Option<String>, String> {
        self.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "The executable chooser is already open")?;
        let _busy = BusyPicker(self.busy.clone());
        std::thread::Builder::new().name("executable-picker".into())
            .spawn(move || choose_file(owner, language)).map_err(|e| format!("Start executable chooser: {e}"))?
            .join().map_err(|_| "Executable chooser worker panicked".to_owned())?
    }
}

fn selected_path(path: &Path) -> Result<String, String> {
    if !path.is_absolute() { return Err("Executable path must be absolute".into()); }
    let path = path.canonicalize().map_err(|e| format!("Cannot open selected executable: {e}"))?;
    if !path.is_file() { return Err("Selected executable must be a file".into()); }
    if !path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| extension.eq_ignore_ascii_case("exe")) {
        return Err("Choose a Windows .exe file".into());
    }
    let path = path.to_str().ok_or("Selected executable path is not valid Unicode")?;
    #[cfg(windows)] {
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") { return Ok(format!(r"\\{unc}")); }
        if let Some(local) = path.strip_prefix(r"\\?\") { return Ok(local.to_owned()); }
    }
    Ok(path.to_owned())
}

#[cfg(windows)]
fn choose_file(owner: isize, language: crate::ui_language::UiLanguage) -> Result<Option<String>, String> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::{
        Foundation::HWND,
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
        UI::Controls::Dialogs::{CommDlgExtendedError, GetOpenFileNameW, OPENFILENAMEW, OFN_DONTADDTORECENT, OFN_ENABLESIZING, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST},
    };
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self) { unsafe { CoUninitialize(); } } }
    // The Shell dialog runs on its own STA, independently of the async pool's
    // apartment. No COM interface pointer crosses a thread boundary.
    let initialized = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
    if initialized < 0 { return Err(format!("Initialize executable chooser COM apartment failed: {initialized:#010x}")); }
    let _apartment = Apartment;
    let title: Vec<u16> = language.text("CLI 실행 파일 선택", "Select CLI executable").encode_utf16().chain(Some(0)).collect();
    let filter: Vec<u16> = language.text("Windows 실행 파일 (*.exe)\0*.exe\0\0", "Windows executable (*.exe)\0*.exe\0\0").encode_utf16().collect();
    let extension: Vec<u16> = "exe\0".encode_utf16().collect();
    let mut buffer = vec![0u16; 32768];
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner as HWND,
        lpstrFilter: filter.as_ptr(),
        nFilterIndex: 1,
        lpstrFile: buffer.as_mut_ptr(),
        nMaxFile: buffer.len() as u32,
        lpstrTitle: title.as_ptr(),
        lpstrDefExt: extension.as_ptr(),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT | OFN_HIDEREADONLY | OFN_ENABLESIZING,
        ..Default::default()
    };
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        // This API supplies a separate error contract; zero is cancellation.
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 { Ok(None) } else { Err(format!("Executable chooser failed with common-dialog error {error:#010x}")) };
    }
    let end = buffer.iter().position(|unit| *unit == 0).ok_or("Executable chooser returned an unterminated path")?;
    let path = OsString::from_wide(&buffer[..end]);
    selected_path(Path::new(&path)).map(Some)
}

#[cfg(not(windows))]
fn choose_file(_: isize, _: crate::ui_language::UiLanguage) -> Result<Option<String>, String> {
    Err("The native executable chooser is supported only on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::selected_path;
    use std::path::Path;

    #[test]
    fn selected_executable_requires_an_existing_absolute_exe_file() {
        assert!(selected_path(Path::new("relative.exe")).is_err());
        let root = std::env::temp_dir().join(format!("agent-grid-executable-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        assert!(selected_path(&root).is_err());
        assert!(selected_path(&root.join("missing.exe")).is_err());
        let shim = root.join("codex.cmd");
        std::fs::write(&shim, "@echo off").unwrap();
        assert!(selected_path(&shim).is_err());
        let unicode = root.join("한글 실행 파일.EXE");
        // This fixture verifies selection/path handling, never PE validation or
        // execution. Settings performs the executable's compatibility check.
        std::fs::write(&unicode, []).unwrap();
        let selected = selected_path(&unicode).unwrap();
        assert_eq!(Path::new(&selected).canonicalize().unwrap(), unicode.canonicalize().unwrap());
        std::fs::remove_file(shim).unwrap();
        std::fs::remove_file(unicode).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
