//! Read-only representative account quota from the selected CLI.
use super::*;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
#[derive(Debug, Default)]
struct UsageRequest {
    last_attempt: Option<Instant>,
    last_success: Option<Instant>,
    last_manual: Option<Instant>,
}
static USAGE_REQUEST: Mutex<UsageRequest> = Mutex::new(UsageRequest {
    last_attempt: None, last_success: None, last_manual: None,
});

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyUsageWindow {
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyUsageSnapshot {
    pub executable: String,
    pub observed_at: i64,
    pub limit_name: Option<String>,
    pub weekly: Option<WeeklyUsageWindow>,
}

fn admission(request: &Mutex<UsageRequest>, now: Instant, manual: bool) -> Result<std::sync::MutexGuard<'_, UsageRequest>, String> {
    let mut permit = request.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => "사용량 조회가 이미 진행 중입니다.".to_owned(),
        std::sync::TryLockError::Poisoned(_) => "사용량 조회 잠금이 손상되었습니다. 앱을 다시 시작하세요.".to_owned(),
    })?;
    if manual {
        if permit.last_manual.is_some_and(|previous| now.saturating_duration_since(previous) < RETRY_INTERVAL) {
            return Err("수동 사용량 조회는 5초 간격으로 가능합니다.".into());
        }
        permit.last_manual = Some(now);
    } else {
        if permit.last_success.is_some_and(|previous| now.saturating_duration_since(previous) < POLL_INTERVAL) {
            return Err("자동 사용량 조회는 1분 간격으로 가능합니다. 새로고침으로 다시 조회할 수 있습니다.".into());
        }
        if permit.last_attempt.is_some_and(|previous| now.saturating_duration_since(previous) < RETRY_INTERVAL) {
            return Err("사용량 조회 재시도는 5초 간격으로 가능합니다.".into());
        }
    }
    permit.last_attempt = Some(now);
    Ok(permit)
}

fn weekly_window(value: &Value) -> Result<Option<WeeklyUsageWindow>, String> {
    // The CLI chooses this backward-compatible representative account bucket.
    // A secondary window is not necessarily weekly; only the explicit duration
    // identifies a week. Missing usage must never become zero usage.
    let limits = value["rateLimits"].as_object().ok_or("Codex returned no account rate-limit snapshot")?;
    let mut weekly = None;
    for key in ["primary", "secondary"] {
        let Some(window) = limits.get(key).filter(|window| !window.is_null()) else { continue; };
        if !window.is_object() { return Err("Codex returned an invalid rate-limit window".into()); }
        if window["windowDurationMins"].as_i64() != Some(10080) { continue; }
        if weekly.is_some() { return Err("Codex returned multiple representative weekly windows".into()); }
        let used_percent = window["usedPercent"].as_f64().filter(|used| used.is_finite() && (0.0..=100.0).contains(used))
            .ok_or("Codex returned an invalid weekly usage percentage")?;
        let resets_at = match window.get("resetsAt") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_i64().filter(|time| *time > 0 && *time <= 8_640_000_000_000)
                .ok_or("Codex returned an invalid weekly reset timestamp")?),
        };
        weekly = Some(WeeklyUsageWindow { used_percent, resets_at });
    }
    Ok(weekly)
}

// Discovery owns short-lived help/version commands before a proxy Job exists.
// One cancellable deadline spans discovery, handshake, initialization and read;
// the proxy's own expiry also interrupts blocked pipe writes after it opens.
struct UsageDeadline {
    cancelled: Arc<AtomicBool>,
    cancel: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}
impl UsageDeadline {
    fn new(deadline: Instant) -> Result<Self, String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let (cancel, receiver) = mpsc::channel();
        let worker = thread::Builder::new().name("usage-read-deadline".into()).spawn(move || {
            if matches!(receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())), Err(mpsc::RecvTimeoutError::Timeout)) {
                flag.store(true, Ordering::Release);
            }
        }).map_err(|e| format!("사용량 조회 기한 감시자를 시작할 수 없습니다: {e}"))?;
        Ok(Self { cancelled, cancel, worker: Some(worker) })
    }
    fn finish(&mut self) -> Result<(), String> {
        // A disconnected receiver already finished; still join its worker.
        let _ = self.cancel.send(());
        self.worker.take().map_or(Ok(()), |worker| worker.join().map_err(|_| "사용량 조회 기한 감시자 정리에 실패했습니다.".into()))
    }
}
impl Drop for UsageDeadline {
    fn drop(&mut self) { let _ = self.finish(); }
}

fn finish_read<T>(result: Result<T, String>, cleanup: Result<(), String>) -> Result<T, String> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; 사용량 조회 연결 정리 실패: {cleanup}")),
    }
}

fn read_snapshot<S: Read + Write>(exe: &OsString, rpc: &mut Rpc<S>) -> Result<WeeklyUsageSnapshot, String> {
    rpc.call("initialize", initialize())?;
    rpc.send(json!({"method":"initialized","params":{}}))?;
    let response = rpc.call("account/rateLimits/read", json!({"excludeResetCreditDetails":true}))?;
    let weekly = weekly_window(&response)?;
    let limit_name = response["rateLimits"]["limitName"].as_str()
        .filter(|name| !name.is_empty() && name.len() <= 512).map(str::to_owned);
    Ok(WeeklyUsageSnapshot {
        executable: exe.to_string_lossy().into_owned(),
        observed_at: SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis() as i64,
        limit_name,
        weekly,
    })
}

fn read_existing(exe: &OsString, cwd: &Path, deadline: Instant) -> Result<WeeklyUsageSnapshot, String> {
    let mut expiry = UsageDeadline::new(deadline)?;
    let result = (|| {
        check_cancelled(Some(&expiry.cancelled))?;
        if Instant::now() >= deadline { return Err("사용량 조회 전체 기한이 만료되었습니다.".into()); }
        let endpoint = shared::SharedEndpoint::existing(exe, cwd, Some(&expiry.cancelled))?
            .ok_or("이 실행 도구에서는 기존 공유 연결로 사용량을 조회할 수 없습니다. 서버를 새로 시작하지 않았습니다.")?;
        let mut proxy = endpoint.open_until(Some(expiry.cancelled.clone()), Some(deadline))?;
        let result = match proxy.rpc.as_mut() {
            Some(rpc) => read_snapshot(exe, rpc),
            None => Err("사용량 조회 연결이 준비되지 않았습니다.".into()),
        };
        finish_read(result, proxy.finish())
    })();
    let result = finish_read(result, expiry.finish());
    if Instant::now() >= deadline {
        finish_read(result, Err("사용량 조회 전체 기한이 만료되었습니다.".into()))
    } else {
        result
    }
}

pub fn weekly_usage_with_executable(exe: &OsString, manual: bool) -> Result<WeeklyUsageSnapshot, String> {
    // ponytail: one nonblocking permit owns the read and its complete process
    // cleanup; a backend cooldown also bounds direct renderer invocations.
    let started = Instant::now();
    let mut permit = admission(&USAGE_REQUEST, started, manual)?;
    let result = env::temp_dir().canonicalize().map_err(|e| format!("사용량 조회 작업 경로를 열 수 없습니다: {e}"))
        .and_then(|cwd| read_existing(exe, &cwd, started + READ_TIMEOUT));
    // A failed read must not consume the successful automatic-read cooldown.
    // The permit remains owned through explicit proxy and watcher settlement.
    if result.is_ok() { permit.last_success = Some(started); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_and_cleanup_failures_are_both_preserved() {
        assert_eq!(finish_read(Ok(42), Ok(())).unwrap(), 42);
        assert_eq!(finish_read::<()>(Err("조회 실패".into()), Ok(())).unwrap_err(), "조회 실패");
        assert_eq!(finish_read(Ok(42), Err("정리 실패".into())).unwrap_err(), "정리 실패");
        let error = finish_read::<()>(Err("조회 실패".into()), Err("정리 실패".into())).unwrap_err();
        assert!(error.contains("조회 실패") && error.contains("정리 실패"));
    }

    #[test]
    fn deadline_workers_are_cancelled_and_joined_without_waiting_for_expiry() {
        let started = Instant::now();
        for _ in 0..32 {
            let mut expiry = UsageDeadline::new(Instant::now() + Duration::from_secs(60)).unwrap();
            expiry.finish().unwrap();
            assert!(expiry.worker.is_none());
            assert!(!expiry.cancelled.load(Ordering::Acquire));
        }
        assert!(started.elapsed() < Duration::from_secs(3), "정상 정산은 전체 조회 기한을 기다리면 안 된다");
        let mut expired = UsageDeadline::new(Instant::now() - Duration::from_millis(1)).unwrap();
        // The worker must actually observe its timeout rather than being cancelled first.
        expired.worker.take().unwrap().join().unwrap();
        assert!(expired.cancelled.load(Ordering::Acquire));
        expired.finish().unwrap();
    }

    fn rpc_message(ws: &mut WebSocket<TcpStream>) -> Value {
        loop { if let Message::Text(text) = ws.read().unwrap() { return serde_json::from_str(&text).unwrap(); } }
    }
    fn rpc_reply(ws: &mut WebSocket<TcpStream>, request: &Value, value: Value) {
        ws.send(Message::Text(json!({"id":request["id"],"result":value}).to_string().into())).unwrap();
    }

    #[test]
    fn usage_connection_only_initializes_and_reads_quota_without_thread_or_server_mutation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut ws = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
            let initialize = rpc_message(&mut ws);
            assert_eq!(initialize["method"], "initialize");
            rpc_reply(&mut ws, &initialize, json!({}));
            let initialized = rpc_message(&mut ws);
            assert_eq!(initialized, json!({"method":"initialized","params":{}}));
            let usage = rpc_message(&mut ws);
            assert_eq!(usage["method"], "account/rateLimits/read");
            assert_eq!(usage["params"], json!({"excludeResetCreditDetails":true}));
            rpc_reply(&mut ws, &usage, json!({"rateLimits":{"limitName":"계정","secondary":{"windowDurationMins":10080,"usedPercent":42.5}}}));
            assert!(ws.read().is_err(), "추가 서버·대화·설정 명령을 보내면 안 된다");
        });
        let mut rpc = Rpc {ws:WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None),next_id:0,cancelled:None,deadline:Some(Instant::now()+Duration::from_secs(2))};
        let snapshot = read_snapshot(&"선택한 실행 도구".into(), &mut rpc).unwrap();
        assert_eq!(snapshot.weekly.unwrap().used_percent, 42.5);
        assert_eq!(snapshot.limit_name.as_deref(), Some("계정"));
        drop(rpc); worker.join().unwrap();
    }

    #[test]
    fn initialization_failure_never_reads_usage_and_cancellation_prevents_all_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut ws = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
            let initialize = rpc_message(&mut ws);
            assert_eq!(initialize["method"], "initialize");
            ws.send(Message::Text(json!({"id":initialize["id"],"error":{"code":-1,"message":"초기화 실패"}}).to_string().into())).unwrap();
            assert!(ws.read().is_err(), "초기화 실패 뒤 사용량 요청을 보내면 안 된다");
        });
        let mut rpc = Rpc {ws:WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None),next_id:0,cancelled:None,deadline:Some(Instant::now()+Duration::from_secs(2))};
        assert!(read_snapshot(&"선택한 실행 도구".into(), &mut rpc).unwrap_err().contains("초기화 실패"));
        drop(rpc); worker.join().unwrap();

        struct NoIo;
        impl Read for NoIo { fn read(&mut self, _: &mut [u8]) -> io::Result<usize> { panic!("취소 뒤 읽기 금지") } }
        impl Write for NoIo {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> { panic!("취소 뒤 쓰기 금지") }
            fn flush(&mut self) -> io::Result<()> { panic!("취소 뒤 flush 금지") }
        }
        let mut rpc = Rpc {ws:WebSocket::from_raw_socket(NoIo, tungstenite::protocol::Role::Client, None),next_id:0,cancelled:Some(Arc::new(AtomicBool::new(true))),deadline:Some(Instant::now()+Duration::from_secs(1))};
        assert!(read_snapshot(&"선택한 실행 도구".into(), &mut rpc).unwrap_err().contains("cancelled"));
    }

    #[test]
    #[cfg(windows)]
    fn missing_or_legacy_shared_server_never_starts_a_backend_and_discovery_obeys_total_deadline() {
        use std::{fs, path::PathBuf};
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::{OpenProcess, WaitForSingleObject}};
        let root = env::temp_dir().join(format!("opendiy-usage-shared-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let source = root.join("fixture.cs");
        fs::write(&source, r#"using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;
class UsageFixture {
 static int Main(string[] args) {
  string root = AppDomain.CurrentDomain.BaseDirectory;
  string mode = File.ReadAllText(Path.Combine(root, "mode")).Trim();
  Console.OutputEncoding = new UTF8Encoding(false);
  File.AppendAllText(Path.Combine(root, "calls"), String.Join(" ", args) + "\n");
  File.AppendAllText(Path.Combine(root, "pids"), Process.GetCurrentProcess().Id.ToString() + "\n");
  if (args.Length == 2 && args[0] == "app-server" && args[1] == "--help") {
   if (mode == "help-stall") Thread.Sleep(30000);
   Console.Write("Usage: fixture app-server [OPTIONS] [COMMAND]\n\n" + (mode == "legacy" ? "Commands:\n  help  Print help\n\n" : "Commands:\n  daemon  Manage daemon\n  proxy  Relay\n\n") + "Options:\n  -h, --help\n          Print help\n");
   return 0;
  }
  if (args.Length == 3 && args[0] == "app-server" && args[1] == "daemon" && args[2] == "version") {
   if (mode == "version-stall") Thread.Sleep(30000);
   Console.Error.Write("fixture shared server is unavailable"); return 23;
  }
  return 99;
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
        let exe = fixture.into_os_string();
        for mode in ["legacy", "offline", "help-stall", "version-stall"] {
            fs::write(root.join("mode"), mode).unwrap();
            fs::write(root.join("calls"), "").unwrap();
            fs::write(root.join("pids"), "").unwrap();
            let started = Instant::now();
            let timeout = if mode.ends_with("stall") { Duration::from_millis(500) } else { Duration::from_secs(5) };
            let error = read_existing(&exe, &root, started + timeout).unwrap_err();
            let calls = fs::read_to_string(root.join("calls")).unwrap();
            let expected = if mode == "legacy" || mode == "help-stall" { "app-server --help\n" } else { "app-server --help\napp-server daemon version\n" };
            assert_eq!(calls, expected, "{mode}: 새 backend·proxy fallback을 실행하면 안 된다");
            assert!(!calls.contains("--listen") && !calls.contains("daemon start"));
            if mode == "legacy" { assert!(error.contains("기존 공유 연결"), "{error}"); }
            else if mode == "offline" { assert!(error.contains("23"), "{error}"); }
            else {
                assert!(error.contains("cancelled"), "{mode}: {error}");
                assert!(started.elapsed() < Duration::from_secs(3), "전체 기한은 discovery부터 적용되어야 한다");
            }
            for pid in fs::read_to_string(root.join("pids")).unwrap().lines() {
                let process = unsafe { OpenProcess(0x00100000, 0, pid.parse().unwrap()) };
                if !process.is_null() {
                    let status = unsafe { WaitForSingleObject(process, 1000) };
                    unsafe { CloseHandle(process); }
                    assert_eq!(status, 0, "소유 discovery 프로세스가 반환 전에 종료되어야 한다");
                }
            }
        }
        // An already expired budget must not even probe the selected executable.
        fs::write(root.join("calls"), "").unwrap();
        assert!(read_existing(&exe, &root, Instant::now() - Duration::from_millis(1)).is_err());
        assert_eq!(fs::read_to_string(root.join("calls")).unwrap(), "");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_explicit_weekly_duration_is_usage_and_missing_is_not_zero() {
        for value in [json!({"rateLimits":{}}), json!({"rateLimits":{"secondary":{"usedPercent":0}}}),
            json!({"rateLimits":{"secondary":{"windowDurationMins":1440,"usedPercent":25}}})] {
            assert!(weekly_window(&value).unwrap().is_none());
        }
        for key in ["primary", "secondary"] {
            let mut value = json!({"rateLimits":{}});
            value["rateLimits"][key] = json!({"windowDurationMins":10080,"usedPercent":0,"resetsAt":2000000000});
            let weekly = weekly_window(&value).unwrap().unwrap();
            assert_eq!(weekly.used_percent, 0.0);
            assert_eq!(weekly.resets_at, Some(2000000000));
        }
        assert!(weekly_window(&json!({"rateLimits":{"secondary":{"windowDurationMins":10080,"usedPercent":32.5,"resetsAt":null}}})).unwrap().unwrap().resets_at.is_none());
    }

    #[test]
    fn malformed_weekly_data_is_rejected_instead_of_fabricated() {
        assert!(weekly_window(&json!({})).is_err());
        for percentage in [Value::Null, json!("42"), json!(-1), json!(101)] {
            assert!(weekly_window(&json!({"rateLimits":{"secondary":{"windowDurationMins":10080,"usedPercent":percentage}}})).is_err());
        }
        for reset in [json!("tomorrow"), json!(-1), json!(0), json!(8_640_000_000_001i64)] {
            assert!(weekly_window(&json!({"rateLimits":{"secondary":{"windowDurationMins":10080,"usedPercent":42,"resetsAt":reset}}})).is_err());
        }
        let window = json!({"windowDurationMins":10080,"usedPercent":42});
        assert!(weekly_window(&json!({"rateLimits":{"primary":window,"secondary":window}})).is_err());
    }

    #[test]
    fn admission_bounds_overlap_auto_interval_and_manual_bypass() {
        let request = Mutex::new(UsageRequest::default());
        let now = Instant::now();
        let mut permit = admission(&request, now, false).unwrap();
        assert!(admission(&request, now + POLL_INTERVAL, false).unwrap_err().contains("진행 중"));
        assert!(admission(&request, now + POLL_INTERVAL, true).unwrap_err().contains("진행 중"));
        permit.last_success = Some(now);
        drop(permit);
        assert!(admission(&request, now + POLL_INTERVAL - Duration::from_millis(1), false).unwrap_err().contains("1분"));
        // The first manual request bypasses the automatic cooldown immediately.
        drop(admission(&request, now, true).unwrap());
        assert!(admission(&request, now + RETRY_INTERVAL - Duration::from_millis(1), true).unwrap_err().contains("5초"));
        drop(admission(&request, now + RETRY_INTERVAL, true).unwrap());
        assert!(admission(&request, now + POLL_INTERVAL, false).is_ok());
    }

    #[test]
    fn failed_read_only_consumes_short_retry_budget() {
        let request = Mutex::new(UsageRequest::default());
        let now = Instant::now();
        drop(admission(&request, now, false).unwrap());
        assert!(admission(&request, now + RETRY_INTERVAL - Duration::from_millis(1), false).unwrap_err().contains("5초"));
        drop(admission(&request, now + RETRY_INTERVAL, false).unwrap());
        // Manual admission also remains bounded after failures.
        drop(admission(&request, now + RETRY_INTERVAL, true).unwrap());
        assert!(admission(&request, now + RETRY_INTERVAL, true).is_err());
        assert!(request.lock().unwrap().last_success.is_none());
    }
}
