//! A child must not create descendants before its owned Job is assigned.
//! The same executable first waits on an unguessable, local named event. Only
//! after Job assignment does the parent release the actual vendor/shell launch.
//! Keep LaunchGate alive until the helper exits so a late-starting helper can
//! still open the named event. No credentials or user config enter this protocol.
#[cfg(windows)]
mod platform {
    use portable_pty::CommandBuilder;
    use std::{ffi::{OsStr, OsString}, os::windows::ffi::OsStrExt, process::{Command, Stdio}};
    use windows_sys::Win32::{Foundation::{CloseHandle, HANDLE}, System::Threading::{CreateEventW, OpenEventW, SetEvent, WaitForSingleObject}};

    pub struct LaunchGate { handle: HANDLE, name: OsString }
    unsafe impl Send for LaunchGate {}
    unsafe impl Sync for LaunchGate {}

    fn wide(value: &OsStr) -> Vec<u16> { value.encode_wide().chain(Some(0)).collect() }

    impl LaunchGate {
        fn new() -> Result<Self, String> {
            let name = OsString::from(format!("Local\\AgentGridLaunch_{}", uuid::Uuid::new_v4().simple()));
            let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide(&name).as_ptr()) };
            if handle.is_null() { return Err(format!("create owned launch gate: {}", std::io::Error::last_os_error())); }
            Ok(Self { handle, name })
        }

        pub fn wrap_pty(mut command: CommandBuilder) -> Result<(CommandBuilder, Self), String> {
            let gate = Self::new()?;
            let original = command.get_argv().to_vec();
            if original.is_empty() { return Err("owned PTY command has no executable".into()); }
            let mut argv = vec![std::env::current_exe().map_err(|e| format!("locate owned PTY helper: {e}"))?.into_os_string(), OsString::from("--pty-child"), gate.name.clone(), OsString::from("--")];
            argv.extend(original);
            *command.get_argv_mut() = argv;
            Ok((command, gate))
        }

        pub fn wrap_std(executable: &OsStr) -> Result<(Command, Self), String> {
            let gate = Self::new()?;
            let mut command = Command::new(std::env::current_exe().map_err(|e| format!("locate owned process helper: {e}"))?);
            command.arg("--owned-child").arg(&gate.name).arg("--").arg(executable);
            Ok((command, gate))
        }

        pub fn release(&self) -> Result<(), String> {
            if unsafe { SetEvent(self.handle) } == 0 { Err(format!("release owned launch gate: {}", std::io::Error::last_os_error())) } else { Ok(()) }
        }
    }
    impl Drop for LaunchGate { fn drop(&mut self) { unsafe { CloseHandle(self.handle); } } }

    pub fn run_child() -> Option<i32> {
        let args: Vec<_> = std::env::args_os().collect();
        let mode = args.get(1)?.to_str()?;
        if !matches!(mode, "--pty-child" | "--owned-child") { return None; }
        let run = || -> Result<i32, String> {
            let name = args.get(2).ok_or("owned child has no launch gate")?;
            if !name.to_string_lossy().starts_with("Local\\AgentGridLaunch_") || !args.get(3).is_some_and(|arg| arg == "--") || args.len() < 5 { return Err("invalid owned launch arguments".into()); }
            // SYNCHRONIZE only: the helper cannot signal its own release.
            let event = unsafe { OpenEventW(0x00100000, 0, wide(name).as_ptr()) };
            if event.is_null() { return Err("owned launch gate is unavailable".into()); }
            let result = unsafe { WaitForSingleObject(event, 15_000) };
            unsafe { CloseHandle(event); }
            if result != 0 { return Err("owned launch gate was not released within 15 seconds".into()); }
            let mut command = Command::new(&args[4]);
            command.args(&args[5..]);
            if mode == "--pty-child" {
                // The release build is a GUI-subsystem executable. ConPTY gives
                // it a console association but may leave its standard handles
                // invalid. Open that exact console, never allocate another one.
                let input = std::fs::OpenOptions::new().read(true).write(true).open("CONIN$").map_err(|e| format!("open owned console input: {e}"))?;
                let output = std::fs::OpenOptions::new().read(true).write(true).open("CONOUT$").map_err(|e| format!("open owned console output: {e}"))?;
                let error = output.try_clone().map_err(|e| e.to_string())?;
                command.stdin(Stdio::from(input)).stdout(Stdio::from(output)).stderr(Stdio::from(error));
            } else {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000); // CREATE_NO_WINDOW for protocol pipes.
            }
            // The helper is already assigned to the session Job; every child
            // inherits it at creation. A helper termination kills descendants.
            let mut child = command.spawn().map_err(|e| format!("launch owned child: {e}"))?;
            match child.wait() {
                Ok(status) => Ok(status.code().unwrap_or(1)),
                Err(error) => { let _ = child.kill(); let _ = child.wait(); Err(format!("reap owned child: {error}")) }
            }
        };
        Some(match run() { Ok(code) => code, Err(error) => { eprintln!("{error}"); 2 } })
    }
}

#[cfg(not(windows))]
mod platform {
    pub struct LaunchGate;
    impl LaunchGate {
        pub fn wrap_pty(command: portable_pty::CommandBuilder) -> Result<(portable_pty::CommandBuilder, Self), String> { Ok((command, Self)) }
        pub fn wrap_std(executable: &std::ffi::OsStr) -> Result<(std::process::Command, Self), String> { Ok((std::process::Command::new(executable), Self)) }
        pub fn release(&self) -> Result<(), String> { Ok(()) }
    }
    pub fn run_child() -> Option<i32> { None }
}
pub use platform::{run_child, LaunchGate};
