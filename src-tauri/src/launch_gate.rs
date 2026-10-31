//! A child must not create descendants before its owned Job is assigned.
//! The console helper first waits on an unguessable, local named event. Only
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

    fn helper_executable() -> Result<std::path::PathBuf, String> {
        let application = std::env::current_exe().map_err(|e| format!("locate OpenDIY Desktop executable: {e}"))?;
        let helper = application.parent().ok_or("OpenDIY Desktop executable has no parent directory")?.join("opendiy-desktop-helper.exe");
        #[cfg(test)]
        let helper = if !helper.is_file() && application.parent().and_then(|path| path.file_name()) == Some(OsStr::new("deps")) {
            application.parent().and_then(|path| path.parent()).ok_or("Test executable has no target directory")?.join("opendiy-desktop-helper.exe")
        } else { helper };
        if !helper.is_file() { return Err(format!("required console helper is missing: {}", helper.display())); }
        Ok(helper)
    }

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
            let mut argv = vec![helper_executable()?.into_os_string(), OsString::from("--pty-child"), gate.name.clone(), OsString::from("--")];
            argv.extend(original);
            *command.get_argv_mut() = argv;
            Ok((command, gate))
        }

        pub fn wrap_std(executable: &OsStr) -> Result<(Command, Self), String> {
            let gate = Self::new()?;
            let mut command = Command::new(helper_executable()?);
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
        // 전용 도우미의 동기 main이며 아직 다른 스레드를 만들지 않았다.
        // 자격은 실제 TUI를 한 번 생성할 때만 넘기고, 도우미 환경에 남기지 않는다.
        let control_keys=["CODEX_DESKTOP_CRON_BOOTSTRAP","CODEX_DESKTOP_CRON_TOKEN"];
        let mut control_env: Vec<_>=control_keys.iter().filter_map(|key| std::env::var_os(key).map(|value|(*key,value))).collect();
        for key in control_keys {std::env::remove_var(key);}
        if mode!="--pty-child" || !args.get(5..).is_some_and(|arguments|arguments.iter().any(|arg|arg=="--desktop-cron-control=pipe-v1")) {control_env.clear();}
        let mut run = || -> Result<i32, String> {
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
            if mode=="--pty-child" && args[5..].iter().any(|arg|arg=="--desktop-cron-control=pipe-v1") {
                command.envs(control_env.drain(..));
            }
            if mode == "--pty-child" {
                // The dedicated helper uses the console subsystem in debug and
                // release, so ConPTY attaches this exact console. Open its
                // handles explicitly; never allocate a separate console.
                let input = std::fs::OpenOptions::new().read(true).write(true).open("CONIN$").map_err(|e| format!("open owned console input: {e}"))?;
                let output = std::fs::OpenOptions::new().read(true).write(true).open("CONOUT$").map_err(|e| format!("open owned console output: {e}"))?;
                let error = output.try_clone().map_err(|e| e.to_string())?;
                command.stdin(Stdio::from(input)).stdout(Stdio::from(output)).stderr(Stdio::from(error));
            } else {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000); // CREATE_NO_WINDOW for protocol pipes.
            }
            // Every child inherits the assigned Job. Only the daemon launcher
            // Job permits the shared server's explicit breakaway from that Job.
            let mut child = command.spawn().map_err(|e| format!("launch owned child: {e}"))?;
            // Command가 보유한 자격 복사본도 실행 직후 해제한다.
            for key in control_keys {command.env_remove(key);}
            drop(command);
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
