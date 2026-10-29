//! 앱 소유 startup capability의 parser·실제 argv·실패 정리 회귀 시험.
use super::*;

#[test]
fn startup_capability_requires_an_exact_declared_option_in_recognized_help() {
    for prefix in ["Usage:", "사용법:"] {
        let normal = format!("Run the server\n\n{prefix} alternate-name app-server [OPTIONS] [COMMAND]\n\nOptions:\n  -h, --help\n          Print help\n      --listen <URI>\n          Server listener\n");
        assert!(!help_supports_no_remote_control(normal.as_bytes()).unwrap(), "{prefix}");
        let supported = format!("{normal}      --no-remote-control\n          Disable automatic startup\n");
        assert!(help_supports_no_remote_control(supported.as_bytes()).unwrap(), "{prefix}");
        assert!(help_supports_no_remote_control(supported.replace('\n', "\r\n").as_bytes()).unwrap(), "{prefix}");
        for suffix in [
            "      --no-remote-control-extra\n          A different option\n",
            "          --no-remote-control is described but not declared\n",
            "\nExamples:\n      --no-remote-control\n",
        ] { assert!(!help_supports_no_remote_control(format!("{normal}{suffix}").as_bytes()).unwrap(), "{prefix}: {suffix}"); }
        for malformed in [
            format!("{prefix} alternate-name [OPTIONS]\nOptions:\n  -h, --help\n"),
            format!("{prefix} alternate-name app-server-extra\nOptions:\n  -h, --help\n"),
            format!("{prefix} alternate-name app-server\nOptions:\n      --no-remote-control\n"),
        ] { assert!(help_supports_no_remote_control(malformed.as_bytes()).is_err(), "{malformed}"); }
    }
    for malformed in ["", "--no-remote-control", "Example: alternate-name app-server\nOptions:\n  -h, --help\n"] {
        assert!(help_supports_no_remote_control(malformed.as_bytes()).is_err(), "{malformed}");
    }
    assert!(help_supports_no_remote_control(&[0xff]).is_err());
}

#[test]
fn enrollment_namespace_requires_a_declared_option_and_never_a_description() {
    let base = "Usage: codex app-server [OPTIONS]\n\nOptions:\n  -h, --help\n          Print help\n";
    assert!(!help_supports_option(base.as_bytes(), "--remote-control-client-name").unwrap());
    for suffix in ["          --remote-control-client-name in description\n", "      --remote-control-client-name-extra <NAME>\n", "\nExamples:\n      --remote-control-client-name <NAME>\n"] {
        assert!(!help_supports_option(format!("{base}{suffix}").as_bytes(), "--remote-control-client-name").unwrap());
    }
    assert!(help_supports_option(format!("{base}      --remote-control-client-name <NAME>\n          Enrollment namespace\n").as_bytes(), "--remote-control-client-name").unwrap());
}
#[test]
#[cfg(windows)]
fn owned_server_startup_probes_help_and_propagates_probe_failures_before_launch() {
    use std::{fs, path::PathBuf};
    use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::{OpenProcess, WaitForSingleObject}};
    let root = env::temp_dir().join(format!("agent-grid-startup-probe-test-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let source = root.join("fixture.cs");
    fs::write(&source, r#"using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;
class StartupFixture {
 static int Main(string[] args) {
  string root = AppDomain.CurrentDomain.BaseDirectory;
  string mode = File.ReadAllText(Path.Combine(root, "mode")).Trim();
  Console.OutputEncoding = new UTF8Encoding(false);
  if (args.Length == 2 && args[0] == "app-server" && args[1] == "--help") {
   File.WriteAllText(Path.Combine(root, "help.args"), String.Join("\n", args));
   File.WriteAllText(Path.Combine(root, "probe.pid"), Process.GetCurrentProcess().Id.ToString());
   if (mode == "failure") { Console.Error.Write("fixture help failed"); return 23; }
   if (mode == "timeout") { Thread.Sleep(30000); return 0; }
   if (mode == "overflow") { Console.Write(new string('x', 8 * 1024 * 1024 + 1024)); Console.Out.Flush(); Thread.Sleep(30000); return 0; }
   if (mode == "invalid-utf8") { Console.OpenStandardOutput().WriteByte(255); return 0; }
   if (mode == "malformed") { Console.Write("not a help document"); return 0; }
   Console.Write((mode.StartsWith("ko-") ? "\uC0AC\uC6A9\uBC95:" : "Usage:") + " fixture app-server [OPTIONS] [COMMAND]\n\nOptions:\n  -h, --help\n          Print help\n      --listen <URI>\n          Listener\n");
   if (mode == "supported" || mode == "ko-supported") Console.Write("      --no-remote-control\n          Disable automatic remote-control startup\n");
   return 0;
  }
  File.WriteAllText(Path.Combine(root, "server.args"), String.Join("\n", args));
  File.WriteAllText(Path.Combine(root, "server.pid"), Process.GetCurrentProcess().Id.ToString());
  if (mode == "cancelled-start") Thread.Sleep(30000);
  return 0;
 }
}
"#).unwrap();
    let compile = root.join("compile.ps1");
    fs::write(&compile, "param([string]$SourcePath,[string]$DestinationPath)\n$ErrorActionPreference='Stop'\nAdd-Type -Path $SourcePath -OutputAssembly $DestinationPath -OutputType ConsoleApplication\n").unwrap();
    let fixture = root.join("fixture.exe");
    let powershell = env::var_os("SystemRoot").map(PathBuf::from).unwrap().join("System32/WindowsPowerShell/v1.0/powershell.exe").into_os_string();
    let compile_args: Vec<String> = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"].into_iter().map(str::to_owned)
        .chain([compile.to_string_lossy().into_owned(), "-SourcePath".into(), source.to_string_lossy().into_owned(), "-DestinationPath".into(), fixture.to_string_lossy().into_owned()]).collect();
    extensions::command_stdout(&powershell, &root, &compile_args, Duration::from_secs(30)).unwrap();
    assert!(fixture.is_file());
    let exe = fixture.into_os_string();
    let exited = |file: &str| {
        let pid: u32 = fs::read_to_string(root.join(file)).unwrap().parse().unwrap();
        let process = unsafe { OpenProcess(0x00100000, 0, pid) }; // SYNCHRONIZE만 사용하며 PID로 종료시키지 않는다.
        if !process.is_null() {
            let status = unsafe { WaitForSingleObject(process, 1000) };
            unsafe { CloseHandle(process); }
            assert_eq!(status, 0, "fixture process {pid} must exit before return");
        }
    };
    // 같은 exe 경로에서 지원을 바꾸어 경로/브랜드/버전 캐시 분기를 배제한다.
    for (mode, supported) in [("unsupported", false), ("supported", true), ("ko-unsupported", false), ("ko-supported", true), ("unsupported", false)] {
        fs::write(root.join("mode"), mode).unwrap();
        let error = launch_server(&exe, &root, None).err().expect("fixture exits instead of implementing WebSocket RPC");
        assert!(error.contains("app-server exited before connecting"), "{error}");
        assert_eq!(fs::read_to_string(root.join("help.args")).unwrap(), "app-server\n--help");
        let actual = fs::read_to_string(root.join("server.args")).unwrap();
        let args: Vec<_> = actual.lines().collect();
        assert_eq!(args.first(), Some(&"app-server"));
        assert_eq!(args.iter().filter(|arg| **arg == "--no-remote-control").count(), usize::from(supported));
        assert!(args.contains(&"--ws-auth") && args.contains(&"--ws-token-sha256"));
        exited("probe.pid"); exited("server.pid");
    }
    for (mode, expected) in [("failure", "23"), ("invalid-utf8", "UTF-8"), ("malformed", "unrecognized"), ("overflow", "exceeded"), ("timeout", "timed out")] {
        fs::write(root.join("mode"), mode).unwrap();
        fs::write(root.join("server.args"), "server was not launched").unwrap();
        let started = Instant::now();
        let error = launch_server(&exe, &root, None).err().expect("failed help must abort startup");
        assert!(error.contains(expected), "{mode}: {error}");
        assert_eq!(fs::read_to_string(root.join("server.args")).unwrap(), "server was not launched");
        assert!(started.elapsed() < HELP_TIMEOUT + Duration::from_secs(5), "probe pipe readers must settle after {mode}");
        exited("probe.pid");
    }
    // 실제 소유 서버가 연결을 기다리는 동안 취소하고 원래 프로세스 핸들로 회수를 확인한다.
    fs::remove_file(root.join("server.pid")).unwrap();
    fs::write(root.join("mode"), "cancelled-start").unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let worker_root = root.clone();
    let worker_exe = exe.clone();
    let worker = thread::spawn(move || launch_server_with_cancellation(&worker_exe, &worker_root, None, Some(worker_cancelled)).err().expect("cancelled server must not become a session"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("server.pid").is_file() {
        assert!(Instant::now() < deadline, "owned cancellation server must start");
        thread::sleep(Duration::from_millis(10));
    }
    let pid: u32 = fs::read_to_string(root.join("server.pid")).unwrap().parse().unwrap();
    let process = unsafe { OpenProcess(0x00100000, 0, pid) };
    assert!(!process.is_null(), "retain the original owned server handle");
    let started = Instant::now();
    cancelled.store(true, Ordering::Release);
    let error = worker.join().unwrap();
    assert!(error.contains("cancelled"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2), "cancelled connect must not wait for the 12-second deadline");
    let status = unsafe { WaitForSingleObject(process, 1000) };
    unsafe { CloseHandle(process); }
    assert_eq!(status, 0, "the original owned server must be reaped before cancellation returns");
    fs::remove_dir_all(root).unwrap();
}
