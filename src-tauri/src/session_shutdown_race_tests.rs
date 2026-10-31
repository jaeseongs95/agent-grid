use super::*;
use std::{fs, net::TcpListener, process::Command, sync::mpsc};
use std::os::windows::{io::{AsRawHandle, FromRawHandle, OwnedHandle}, process::CommandExt};
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
use serde_json::{json, Value};
use tungstenite::Message;

fn request() -> StartRequest {
    StartRequest {cwd:std::env::current_dir().unwrap().to_string_lossy().into_owned(),title:"종료 경합 시험".into(),project_id:None,project_assigned:false,thread_id:None,cols:80,rows:24}
}
fn until(label: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !ready() { assert!(Instant::now() < deadline, "시험 경계에 도달하지 못했다: {label}"); thread::sleep(Duration::from_millis(2)); }
}

#[test]
fn shutdown_rechecks_completed_unknown_without_remote_retransmission_and_restores_admission() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
    let failed = engine.allocate(&request()).unwrap();
    failed.request_stop().unwrap();
    failed.cleanup_failed.store(true, Ordering::Release);
    failed.finish("error").unwrap();
    for _ in 0..2 {
        assert!(engine.shutdown().unwrap_err().contains("재전송하지 않았습니다"), "완료된 불명도 종료 실패여야 한다");
        assert_eq!(failed.info().unwrap().state, "unknown");
        assert!(failed.process.lock().unwrap().is_none(), "완료 검사에는 전송 자원이 없다");
        assert!(!engine.inner.exiting.load(Ordering::Acquire));
        let next = engine.allocate(&request()).expect("종료 실패 뒤 새 세션 입장을 복원해야 한다");
        next.finish("exited").unwrap();
    }
}

#[test]
fn shutdown_keeps_unknown_that_finishes_between_cancellation_and_worker_selection() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let target = Arc::new(Mutex::new(None::<std::sync::Weak<Session>>));
    let callback_target = target.clone();
    let finish_once = AtomicBool::new(false);
    let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| {
        if let BackendEvent::Changed(info) = event {
            if info.state == "stopping" && !finish_once.swap(true, Ordering::AcqRel) {
                let session = callback_target.lock().unwrap().as_ref().unwrap().upgrade().unwrap();
                session.cleanup_failed.store(true, Ordering::Release);
                session.finish("error")?;
            }
        }
        Ok(())
    })).unwrap();
    let session = engine.allocate(&request()).unwrap();
    *target.lock().unwrap() = Some(Arc::downgrade(&session));
    assert!(engine.shutdown().unwrap_err().contains("재전송하지 않았습니다"), "취소 발행 직후 완료된 불명을 worker 필터로 누락하면 안 된다");
    assert_eq!(session.info().unwrap().state, "unknown");
    assert!(!engine.inner.exiting.load(Ordering::Acquire));
}

#[test]
fn concurrent_shutdown_refusal_cannot_reopen_admission_of_the_winner() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    for fail in [false, true] {
        let target = Arc::new(Mutex::new(None::<std::sync::Weak<Session>>));
        let callback_target = target.clone();
        let failed_once = AtomicBool::new(false);
        let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| {
            if let BackendEvent::Changed(info) = event {
                if fail && info.state == "stopping" && !failed_once.swap(true, Ordering::AcqRel) {
                    let session = callback_target.lock().unwrap().as_ref().unwrap().upgrade().unwrap();
                    session.finish("exited")?;
                    return Err("합성 종료 상태 전달 실패".into());
                }
            }
            Ok(())
        })).unwrap();
        if fail {
            let session = engine.allocate(&request()).unwrap();
            *target.lock().unwrap() = Some(Arc::downgrade(&session));
        }
        let registry = engine.inner.sessions.lock().unwrap();
        let closing = engine.clone();
        let winner = thread::spawn(move || closing.shutdown());
        until("종료 입장 차단", || engine.inner.exiting.load(Ordering::Acquire));
        assert!(engine.shutdown().unwrap_err().contains("extension management is busy"));
        assert!(engine.inner.exiting.load(Ordering::Acquire), "거절된 동시 종료는 먼저 진입한 종료의 입장 차단을 풀면 안 된다");
        drop(registry);
        let result = winner.join().unwrap();
        assert_eq!(result.is_err(), fail);
        assert_eq!(engine.inner.exiting.load(Ordering::Acquire), !fail);
        if fail {
            let session = engine.allocate(&request()).unwrap();
            session.finish("exited").unwrap();
        } else {
            assert!(matches!(engine.allocate(&request()), Err(error) if error.contains("application is exiting")));
        }
    }
}

// 실제 Proxy/관측자/PTY/reaper를 통과하는 합성 CLI다. 일반 파일은 소켓 세대
// 검사용 표식일 뿐이며 인증·실제 daemon·모델 호출 또는 보안 검증을 대신하지 않는다.
fn compile_cli(root: &Path) -> PathBuf {
    let source = root.join("fixture.cs");
    fs::write(&source, r#"using System;
using System.IO;
using System.Net.Sockets;
using System.Text;
using System.Threading;
class Fixture {
 static int Main(string[] args) {
  string root = AppDomain.CurrentDomain.BaseDirectory;
  if (args.Length == 1 && args[0] == "--help") {
   Console.Write("Usage: fixture [OPTIONS]\n\nOptions:\n  -h, --help\n          Print help\n"); return 0;
  }
  if (args.Length == 2 && args[0] == "app-server" && args[1] == "--help") {
   Console.Write("Usage: fixture app-server [OPTIONS] [COMMAND]\n\nCommands:\n  daemon  Manage daemon\n  proxy  Relay\n\nOptions:\n  -h, --help\n          Print help\n"); return 0;
  }
  if (args.Length == 3 && args[0] == "app-server" && args[1] == "daemon" && args[2] == "version") { Console.Write(File.ReadAllText(Path.Combine(root,"version.json"))); return 0; }
  if (args.Length == 2 && args[0] == "app-server" && args[1] == "proxy") {
   using (var client = new TcpClient("127.0.0.1",Int32.Parse(File.ReadAllText(Path.Combine(root,"port"))))) {
    var stream=client.GetStream();
    var input=new Thread(()=> { try { Console.OpenStandardInput().CopyTo(stream); } catch {} }); input.IsBackground=true; input.Start();
    try { stream.CopyTo(Console.OpenStandardOutput()); } catch {} return 0;
   }
  }
  if (args.Length > 0 && args[0] == "resume") {
   File.WriteAllText(Path.Combine(root,"tui.pid.pending"),System.Diagnostics.Process.GetCurrentProcess().Id.ToString());
   File.Move(Path.Combine(root,"tui.pid.pending"),Path.Combine(root,"tui.pid"));
   DateTime deadline=DateTime.UtcNow.AddSeconds(15);
   while (!File.Exists(Path.Combine(root,"tui.exit")) && DateTime.UtcNow < deadline) Thread.Sleep(10);
   return 0;
  }
  return 23;
 }
}
"#).unwrap();
    let executable = root.join("fixture.exe");
    let compiler = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
    let output = Command::new(compiler).arg("/nologo").arg(format!("/out:{}", executable.display())).arg(&source).creation_flags(0x08000000).output().unwrap();
    assert!(output.status.success(), "합성 CLI 컴파일 실패: {}", String::from_utf8_lossy(&output.stdout));
    executable
}

#[test]
fn shared_reaper_handoff_during_observer_join_reports_unknown_for_stop_and_shutdown() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!("opendiy-shutdown-race-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let executable = compile_cli(&root);
    let cwd = root.canonicalize().unwrap();
    fs::write(root.join("socket"), b"synthetic rendezvous").unwrap();
    fs::write(root.join("version.json"), json!({"status":"running","socketPath":cwd.join("socket"),"appServerVersion":"fixture"}).to_string()).unwrap();
    let stage = |name: &str| fs::write(root.join("stage"), name).unwrap();
    for shutdown in [false, true] {
        stage(if shutdown {"shutdown 초기화"} else {"stop 초기화"});
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(root.join("port"), listener.local_addr().unwrap().port().to_string()).unwrap();
        let no_more_connections = listener.try_clone().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server_cwd = cwd.clone();
        let server = thread::spawn(move || {
            let mut connections = Vec::new();
            // 시작 전 준비가 짧은 초기화 연결을 닫은 뒤 실제 세션 관측자가 별도로 연결한다.
            for preparation in [true, false] {
            let mut accepted = None;
            until(if preparation {"준비 proxy 연결"} else {"관측 proxy 연결"}, || match listener.accept() { Ok(pair) => {accepted=Some(pair);true}, Err(e) if e.kind()==std::io::ErrorKind::WouldBlock => false, Err(e) => panic!("{e}") });
            let (stream, _) = accepted.unwrap();
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let id = Uuid::new_v4().to_string();
            let mut methods = Vec::new();
            while let Ok(message) = ws.read() {
                let Message::Text(text) = message else { continue; };
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap().to_owned();
                methods.push(method.clone());
                let result = match method.as_str() {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    _ if preparation => panic!("사전 준비 연결이 대화 변경 또는 관측을 요청했다: {method}"),
                    "thread/start" => json!({"thread":{"id":id,"cwd":server_cwd,"status":{"type":"idle"}}}),
                    "thread/subscribePassive" => {
                        assert_eq!(methods.iter().filter(|method|method.as_str()=="thread/subscribePassive").count(),1,"수동 관측 구독은 한 번만 허용한다");
                        assert_eq!(request["params"],json!({"threadId":id}),"원래 대화의 수동 구독 외 변경은 허용하지 않는다");
                        json!({"thread":{"id":id,"cwd":server_cwd,"turns":[],"status":{"type":"active"}}})
                    },
                    "thread/read" => json!({"thread":{"id":id,"cwd":server_cwd,"status":{"type":"active"}}}),
                    _ => panic!("예상하지 않은 변경 요청: {method}"),
                };
                ws.send(Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).unwrap();
            }
            if preparation { assert_eq!(methods, ["initialize", "initialized"], "사전 준비는 초기화만 하고 연결을 닫아야 한다"); }
            connections.push(methods);
            }
            connections
        });
        let (entered, entry) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let blocked = AtomicBool::new(false);
        let events = Arc::new(Mutex::new(Vec::new()));
        let (output_events, output_rx) = mpsc::channel();
        let callback_events = events.clone();
        let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| {
            if let BackendEvent::Output(chunk) = &event { output_events.send(chunk.clone()).unwrap(); }
            if let BackendEvent::Changed(info) = event {
                callback_events.lock().unwrap().push(info.state.clone());
                if info.state == "running" && thread::current().name().is_none() && !blocked.swap(true, Ordering::AcqRel) {
                    entered.send(()).unwrap();
                    released.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
                }
            }
            Ok(())
        })).unwrap();
        engine.inner.store.lock().unwrap().set_codex_executable(Some(executable.to_str().unwrap())).unwrap();
        let mut start = request(); start.cwd = cwd.to_string_lossy().into_owned();
        stage("launch 진입");
        let started = engine.start(start).unwrap();
        stage("launch 반환");
        entry.recv_timeout(Duration::from_secs(8)).unwrap();
        let session = engine.lookup(&started.id, started.generation).unwrap();
        let process = session.process.lock().unwrap().as_ref().unwrap().clone();
        let mut output = String::new();
        let mut answered = 0;
        until("TUI PID 기록", || {
            for chunk in output_rx.try_iter() {
                engine.acknowledge(&chunk.id, chunk.generation, chunk.seq).unwrap();
                output.push_str(&chunk.data);
                // 실제 xterm처럼 프레임 경계를 넘는 ConPTY 커서 질의도 응답한다.
                let queries = output.matches("\x1b[6n").count();
                while answered < queries { engine.write(&started.id, started.generation, "\x1b[1;1R").unwrap(); answered += 1; }
            }
            root.join("tui.pid").exists()
        });
        // WriteAllText의 생성과 내용 기록 사이를 준비 완료로 오인하지 않는다.
        // 닫힌 임시 파일의 rename 게시 뒤에만 원래 TUI PID를 읽는다.
        assert!(!root.join("tui.pid.pending").exists(), "PID 원문이 완전히 기록된 뒤 게시되어야 한다");
        let pid: u32 = fs::read_to_string(root.join("tui.pid")).unwrap().parse().unwrap();
        let handle = unsafe { OpenProcess(0x00100000, 0, pid) };
        assert!(!handle.is_null());
        let original = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
        stage("TUI 종료 신호");
        fs::write(root.join("tui.exit"), b"exit").unwrap();
        until("reaper adapter 회수", || process.adapter.lock().unwrap().is_none());
        stage("adapter 회수 관측");
        assert_eq!(unsafe { WaitForSingleObject(original.as_raw_handle().cast(), 1000) }, 0);
        assert!(session.phase.load(Ordering::Acquire) < 3, "관측자 join 중의 실제 reaper 창이어야 한다");
        let closing = engine.clone();
        let worker = thread::spawn(move || if shutdown {closing.shutdown()} else {closing.stop(&started.id, started.generation)});
        until("중단 요청 발행", || session.stop_requested.load(Ordering::Acquire));
        stage("중단 요청 관측");
        release.send(()).unwrap();
        stage("observer 해제");
        let result = worker.join().unwrap();
        assert!(result.is_err(), "adapter 회수 뒤 취소를 중단 성공으로 보고하면 안 된다: state={}", session.info().unwrap().state);
        assert!(result.unwrap_err().contains("재전송하지 않았습니다"));
        stage("stop 반환");
        assert_eq!(session.info().unwrap().state, "unknown");
        assert!(!session.info().unwrap().active);
        assert!(session.process.lock().unwrap().is_none());
        assert!(!events.lock().unwrap().iter().any(|state| state == "stopped"));
        assert!(engine.shutdown().is_err());
        assert!(engine.shutdown().is_err());
        assert!(!engine.inner.exiting.load(Ordering::Acquire));
        stage("재시도 반환");
        assert_eq!(server.join().unwrap(), [vec!["initialize", "initialized"], vec!["initialize", "initialized", "thread/start", "thread/read", "thread/subscribePassive"]]);
        stage("server 회수");
        no_more_connections.set_nonblocking(true).unwrap();
        assert!(matches!(no_more_connections.accept(), Err(e) if e.kind()==std::io::ErrorKind::WouldBlock), "불명 요청을 다시 연결·전송하면 안 된다");
        stage("추가 연결 없음");
        fs::remove_file(root.join("tui.pid")).unwrap();
        fs::remove_file(root.join("tui.exit")).unwrap();
    }
    for name in ["fixture.cs","fixture.exe","socket","version.json","port","stage"] {fs::remove_file(root.join(name)).unwrap();}
    fs::remove_dir(root).unwrap();
}
