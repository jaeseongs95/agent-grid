//! 공유 서버 연결과 앱 시작 준비. 설치·교체·종료·원격 등록은 바꾸지 않는다.
use super::*;
use std::{fs, path::PathBuf};
const STOP_TIMEOUT: Duration = Duration::from_secs(8);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(40);
static STARTUP_REQUEST: Mutex<()> = Mutex::new(());

/// 앱 시작의 명시적 준비 경로다. 카탈로그 조회·개별 연결은 서버를 시작하지 않는다.
pub(super) fn prepare_runtime(exe: &OsString, cwd: &Path, cancelled: &Arc<AtomicBool>) -> Result<(), String> {
    let _request = STARTUP_REQUEST.try_lock().map_err(|_| "공유 서버를 준비 중입니다. 잠시 후 다시 시도하세요.")?;
    check_cancelled(Some(cancelled))?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let value = prepare_runtime_commands(
        |args| extensions::command_stdout_until(exe, cwd, &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(), deadline, Some(cancelled)),
        || extensions::start_daemon_stdout_until(exe, cwd, deadline, Some(cancelled)),
    )?;
    let Some(value) = value else { return Ok(()); }; // 명시 미지원 CLI의 기존 개별 서버 경로 유지.
    check_cancelled(Some(cancelled))?;
    let socket = lifecycle_socket(&value)?;
    let endpoint = SharedEndpoint { exe: exe.clone(), stamp: socket_stamp(&socket)?, socket };
    let mut proxy = endpoint.open_until(Some(cancelled.clone()), Some(deadline))?;
    let ready = initialize_proxy(&mut proxy).and_then(|_| check_cancelled(Some(cancelled)));
    let cleanup = proxy.finish();
    match (ready, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; 준비 연결 정리 실패: {cleanup}")),
    }
}

// 명령 경계를 주입하여 실행 순서·시작 횟수·실패 후 복원 금지를 합성 응답으로 검사한다.
fn prepare_runtime_commands(
    mut query: impl FnMut(&[&str]) -> Result<Vec<u8>, String>,
    start: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<Option<Value>, String> {
    let help = query(&["app-server", "--help"])?;
    if !supports_shared(&help)? { return Ok(None); }
    let version = ["app-server", "daemon", "version"];
    match query(&version) {
        Ok(bytes) => {
            let value = lifecycle_value(&bytes)?;
            lifecycle_socket(&value)?;
            Ok(Some(value))
        }
        Err(probe) => {
            let help = query(&["app-server", "daemon", "start", "--help"])
                .map_err(|error| format!("공유 서버 시작 지원을 확인하지 못했습니다: {error}; 실행 상태 확인: {probe}"))?;
            if !help_supports_option(&help, "--existing-installation")? {
                return Err("선택한 CLI는 기존 설정을 보존하는 공유 서버 시작을 지원하지 않습니다. 실행 파일을 업데이트한 뒤 다시 시도하세요.".into());
            }
            let started = lifecycle_value(&start().map_err(|error| format!("공유 서버 시작 실패: {error}"))?)?;
            if !matches!(started["status"].as_str(), Some("started" | "alreadyRunning")) {
                return Err("공유 서버 시작 결과를 확인할 수 없습니다. 세션을 복원하지 않았습니다.".into());
            }
            let mut running = started.clone();
            running["status"] = json!("running");
            let started_socket = lifecycle_socket(&running)?;
            // start 자체의 ready 대기 뒤 실제 version 응답과 같은 endpoint인지 재확인한다.
            let ready = lifecycle_value(&query(&version).map_err(|error| format!("공유 서버가 연결 가능한 상태인지 확인하지 못했습니다: {error}"))?)?;
            if lifecycle_socket(&ready)? != started_socket {
                return Err("준비 중 공유 서버 연결 경로가 바뀌었습니다. 세션을 복원하지 않았습니다.".into());
            }
            Ok(Some(ready))
        }
    }
}
fn lifecycle_value(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|_| "공유 서버 상태 응답이 올바른 JSON이 아닙니다.".into())
}

#[derive(Clone)]
pub(super) struct SharedEndpoint {
    exe: OsString,
    socket: PathBuf,
    stamp: SocketStamp,
}
#[derive(Clone, PartialEq, Eq)]
struct SocketStamp {
    canonical: PathBuf,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
}
fn socket_stamp(path: &Path) -> Result<SocketStamp, String> {
    // Windows AF_UNIX 소켓은 일반 파일처럼 열거나 canonicalize할 수 없다.
    // rendezvous 자체의 메타데이터와 부모의 정규 경로만 읽으며 ACL 검사는 CLI에 맡긴다.
    let metadata = fs::symlink_metadata(path).map_err(|e| format!("공유 서버 소켓을 확인할 수 없습니다: {e}"))?;
    let parent = path.parent().ok_or("공유 서버 소켓의 상위 경로가 없습니다.")?;
    let name = path.file_name().ok_or("공유 서버 소켓 파일 이름이 없습니다.")?;
    Ok(SocketStamp {
        canonical: parent.canonicalize().map_err(|e| format!("공유 서버 소켓 경로를 확인할 수 없습니다: {e}"))?.join(name),
        created: metadata.created().ok(), modified: metadata.modified().ok(),
    })
}
impl SharedEndpoint {
    pub(super) fn existing(exe: &OsString, cwd: &Path, cancelled: Option<&Arc<AtomicBool>>) -> Result<Option<Self>, String> {
        // 정상 도움말에서 명시 미지원이 확인된 구형 CLI만 개별 서버를 사용한다.
        // 실행·권한·프로토콜 오류를 미지원으로 삼켜 다른 서버로 대체하지 않는다.
        let help_args = ["app-server", "--help"].map(str::to_owned);
        let deadline = Instant::now() + HELP_TIMEOUT;
        let help = extensions::command_stdout_until(exe, cwd, &help_args, deadline, cancelled)
            .map_err(|e| format!("Codex 공유 연결 지원을 확인할 수 없습니다: {e}"))?;
        if !supports_shared(&help)? { return Ok(None); }
        let args = ["app-server", "daemon", "version"].map(str::to_owned);
        let output = extensions::command_stdout_until(exe, cwd, &args, deadline, cancelled)
            .map_err(|e| format!("기존 공유 Codex 서버에 연결할 수 없습니다. Codex에서 공유 서버의 실행 상태와 권한을 확인한 뒤 다시 시도하세요. 서버를 자동으로 설치하거나 시작하지 않았습니다. 원인: {e}"))?;
        let value: Value = serde_json::from_slice(&output).map_err(|_| "공유 서버 버전 응답이 올바른 JSON이 아닙니다.")?;
        let socket = lifecycle_socket(&value)?;
        let stamp = socket_stamp(&socket)?;
        Ok(Some(Self { exe:exe.clone(), socket, stamp }))
    }
    pub(super) fn open(&self, cancelled: Option<Arc<AtomicBool>>) -> Result<Proxy, String> {
        self.open_until(cancelled, None)
    }
    pub(super) fn open_until(&self, cancelled: Option<Arc<AtomicBool>>, deadline: Option<Instant>) -> Result<Proxy, String> {
        self.verify()?;
        // 무인자 default proxy 분기의 private 디렉터리/비상승 상대 검사를 사용한다.
        // TUI는 같은 CLI의 daemon/version이 반환한 기본 소켓을 명시한다.
        let proxy = Proxy::open_until(&self.exe, cancelled, deadline)?;
        if let Err(error)=self.verify() {return match proxy.finish(){Ok(())=>Err(error),Err(cleanup)=>Err(format!("{error}; 소유 proxy 정리 실패: {cleanup}"))};}
        Ok(proxy)
    }
    fn verify(&self) -> Result<(), String> {
        if socket_stamp(&self.socket)? != self.stamp {
            return Err("공유 서버 소켓이 바뀌었습니다. 세션을 다시 시작하세요. 다른 서버로 연결을 대체하지 않았습니다.".into());
        }
        Ok(())
    }
    fn tui(&self, cwd: &Path, id: &str) -> Result<CommandBuilder, String> {
        let socket = self.socket.to_str().ok_or("공유 서버 소켓 경로가 유효한 문자열이 아닙니다.")?;
        let mut command = CommandBuilder::new(&self.exe);
        command.args(["resume", id, "--remote", &format!("unix://{socket}"), "--cd"]);
        command.arg(cwd.as_os_str()); command.cwd(cwd);
        // unix 연결에는 WS 전용 토큰 옵션을 전달하지 않는다. 기본 daemon의 명시적
        // endpoint이므로 공식 TUI가 private ACL을 확인하며 embedded 대체를 금지한다.
        Ok(command)
    }
}
fn supports_shared(bytes: &[u8]) -> Result<bool, String> {
    // 기존 도움말 검증기로 usage와 options 선언을 먼저 확인한다.
    help_supports_option(bytes, "--help")?;
    let help = std::str::from_utf8(bytes).map_err(|_| "Codex 도움말이 올바른 UTF-8이 아닙니다.")?;
    let mut commands = help.lines();
    if !commands.any(|line| matches!(line.trim(), "Commands:" | "명령:" | "명령어:")) {
        if help.lines().any(|line| line.trim_start().starts_with("Usage:") || line.trim_start().starts_with("사용법:"))
            && help.lines().any(|line| line.split_ascii_whitespace().any(|token| token == "[COMMAND]")) {
            return Err("Codex 도움말의 명령 목록 형식을 확인할 수 없습니다. 다른 서버로 대체하지 않았습니다.".into());
        }
        return Ok(false);
    }
    let lines: Vec<_> = commands.take_while(|line| line.is_empty() || line.starts_with(char::is_whitespace)).filter(|line| !line.trim().is_empty()).collect();
    let column = lines.iter().map(|line| line.len() - line.trim_start().len()).min().unwrap_or(0);
    let declared = |name: &str| lines.iter().any(|line| line.len() - line.trim_start().len() == column && line.split_ascii_whitespace().next() == Some(name));
    Ok(declared("daemon") && declared("proxy"))
}
fn lifecycle_socket(value: &Value) -> Result<PathBuf, String> {
    if value["status"] != "running" || !value["appServerVersion"].as_str().is_some_and(|v| !v.is_empty()) {
        return Err("공유 서버의 실행 상태와 버전을 확인할 수 없습니다.".into());
    }
    let socket = value["socketPath"].as_str().filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or("공유 서버 소켓 경로가 누락되었습니다.")?;
    let path = PathBuf::from(socket);
    if !path.is_absolute() { return Err("공유 서버 소켓 경로는 절대 경로여야 합니다.".into()); }
    Ok(path)
}

#[derive(Clone)]
pub(crate) struct SharedControl {
    endpoint: SharedEndpoint,
    thread_id: String,
    cwd: PathBuf,
}
impl SharedControl {
    /// 세대가 확인된 Desktop 세션에서 PTY를 종료하기 전에 원래 작업 ID를 고정한다.
    pub(crate) fn capture_stop(&self, deadline: Instant) -> Result<SharedStop, String> {
        let mut proxy = self.endpoint.open_until(None, Some(deadline.min(Instant::now() + STOP_TIMEOUT)))?;
        let rpc = proxy.rpc.as_mut().unwrap();
        let turn_id = capture_supported_stop(rpc, &self.thread_id, &self.cwd)?;
        Ok(SharedStop {proxy,thread_id:self.thread_id.clone(),cwd:self.cwd.clone(),turn_id})
    }
}
pub(crate) struct SharedStop {
    proxy: Proxy,
    thread_id: String,
    cwd: PathBuf,
    turn_id: Option<String>,
}
impl SharedStop {
    pub(crate) fn finish(mut self) -> Result<(), String> {
        // 이 값은 지원을 확인한 동일 연결만 소유한다. 단절 시 재연결·재전송하지 않는다.
        interrupt_exact(self.proxy.rpc.as_mut().unwrap(), &self.thread_id, &self.cwd, self.turn_id.as_deref())
    }
}
fn capture_supported_stop<S: Read + Write>(rpc: &mut Rpc<S>, thread_id: &str, cwd: &Path) -> Result<Option<String>, String> {
    // 닫기 전용 연결에는 passive 구독이 없다. 관찰자나 다른 연결의 지원을 빌리지 않는다.
    // false와 누락도 기존 오류 경로로 전달하여 자기 PTY 정리 후 미확인 상태를 보존한다.
    if !initialize_stop_rpc(rpc)? {
        return Err("공유 서버의 정확한 작업 취소 지원을 확인하지 못했습니다. 취소 요청을 보내지 않았으며 중단 결과는 확인되지 않았습니다.".into());
    }
    capture_turn(rpc, thread_id, cwd)
}
fn capture_turn<S: Read + Write>(rpc: &mut Rpc<S>, thread_id: &str, cwd: &Path) -> Result<Option<String>, String> {
        let thread = read_state(rpc, Some(thread_id), cwd)?.ok_or("중단 대상 대화가 없습니다.")?;
        match thread["status"]["type"].as_str() {
            Some("idle" | "notLoaded") => return Ok(None),
            Some("active") => {},
            _ => return Err("공유 대화의 현재 작업 상태를 확인할 수 없습니다.".into()),
        }
        let page = rpc.call("thread/turns/list", json!({"threadId":thread_id,"limit":1,"sortDirection":"desc","itemsView":"summary"}))?;
        let turn_id = active_turn(&page)?;
        turn_id.map(|id| Some(id.to_owned())).ok_or_else(|| "공유 대화가 실행 중이지만 현재 작업 ID를 확인할 수 없어 중단하지 않았습니다.".into())
}
fn interrupt_exact<S: Read + Write>(rpc: &mut Rpc<S>, thread_id: &str, cwd: &Path, captured: Option<&str>) -> Result<(), String> {
    if let Some(id) = captured {
        // 중단 사이에 모바일이 시작한 새 turn ID를 다시 조회하거나 대체하지 않는다.
        rpc.call("turn/interrupt", json!({"threadId":thread_id,"turnId":id}))?;
    } else {
        let thread = read_state(rpc, Some(thread_id), cwd)?.ok_or("중단 대상 대화가 없습니다.")?;
        if !matches!(thread["status"]["type"].as_str(), Some("idle" | "notLoaded")) {
            return Err("세션 정리 중 공유 대화에 새 작업이 관측되었습니다. 새 작업은 중단하지 않았으며 중단 결과는 확인되지 않았습니다.".into());
        }
    }
    Ok(())
}
fn active_turn(page: &Value) -> Result<Option<&str>, String> {
    let data = page["data"].as_array().ok_or("공유 작업 목록 응답이 올바르지 않습니다.")?;
    if data.len() > 1 { return Err("공유 작업 목록이 요청한 상한을 넘었습니다.".into()); }
    let Some(turn) = data.first() else { return Ok(None); };
    if turn["status"] != "inProgress" { return Ok(None); }
    turn["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(Some).ok_or_else(|| "공유 작업 ID가 올바르지 않습니다.".into())
}
fn initialize_stop_rpc<S: Read + Write>(rpc: &mut Rpc<S>) -> Result<bool, String> {
    let response = rpc.call("initialize", initialize())?;
    let fields = response.as_object().ok_or("공유 서버 초기화 응답이 객체가 아닙니다.")?;
    let supported = match fields.get("supportsExpectedTurnInterrupt") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return Err("공유 서버의 작업 중단 지원 정보 형식이 올바르지 않아 취소 요청을 보내지 않았습니다. 정확한 작업 취소를 확인하지 못했습니다.".into()),
    };
    rpc.send(json!({"method":"initialized","params":{}}))?;
    Ok(supported)
}
fn initialize_proxy(proxy: &mut Proxy) -> Result<(), String> {
    proxy.call("initialize", initialize())?;
    proxy.rpc.as_mut().unwrap().send(json!({"method":"initialized","params":{}}))
}

pub(super) fn prepare(cwd: &Path, requested: Option<&str>, endpoint: SharedEndpoint,
    on_event: Arc<dyn Fn(CodexEvent) + Send + Sync>, cancelled: Arc<AtomicBool>) -> Result<CodexSession, String> {
    check_cancelled(Some(&cancelled))?;
    let mut proxy = endpoint.open(Some(cancelled.clone()))?;
    initialize_proxy(&mut proxy)?;
    let thread = if let Some(id) = requested {
        proxy.call("thread/read", json!({"threadId":id,"includeTurns":false}))?["thread"].clone()
    } else {
        // 새 대화 ID를 명시적으로 받아 외부 loaded root를 채택하지 않는다.
        let result = proxy.call("thread/start", json!({"cwd":cwd,"experimentalRawEvents":false,"persistExtendedHistory":false}))?;
        let thread = result["thread"].clone();
        // 빈 대화가 TUI 연결 전에 무구독 idle 정리로 사라지지 않도록 이 구독을
        // 세션 수명 동안 유지한다. 관측자는 입력·승인 응답을 보내지 않으며 공식
        // 서버의 일반 승인 요청은 TUI를 포함한 모든 구독자에게 전달된다.
        thread
    };
    let id = thread["id"].as_str().ok_or("공유 대화 ID가 누락되었습니다.")?.to_owned();
    Uuid::parse_str(&id).map_err(|_| "공유 대화 ID가 UUID가 아닙니다.")?;
    verified_thread(&thread, requested.unwrap_or(&id), cwd)?;
    let command = endpoint.tui(cwd, &id)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let control = SharedControl { endpoint:endpoint.clone(),thread_id:id.clone(),cwd:cwd.into() };
    let conversation = Some(ConversationConnection::new(super::conversation::Endpoint::Shared(endpoint.clone()), stopped.clone()));
    let mobile = MobileConnection::shared(endpoint, stopped.clone());
    on_event(CodexEvent::ThreadId(id.clone()));
    on_event(CodexEvent::Status(status(&thread["status"])));
    let observing = stopped.clone();
    let observer_cwd = cwd.to_path_buf();
    let mut rpc = proxy.rpc.take().unwrap();
    let proxy_job = proxy.job.as_ref().map(Arc::downgrade);
    // 초기 취소와 실행 중 정리 둘 다 같은 RPC 취소 경계를 사용한다.
    rpc.cancelled = Some(stopped.clone());
    check_cancelled(Some(&cancelled))?;
    // 시작 중 포화 쓰기를 감시한 임시 작업자는 여기서 회수한다. 장기 관측은
    // 세션의 Weak<Job> 정리로 쓰기를 해제하므로 세션마다 감시 스레드를 남기지 않는다.
    proxy.expiry.take();
    let observed_conversation = conversation.clone();
    let observer = thread::spawn(move || {
        observe(&mut rpc, Some(id), &observer_cwd, &observing, on_event.as_ref(),observed_conversation.as_ref());
        drop(rpc); // receiver를 먼저 닫아 bounded pipe reader의 send를 깨운다.
        drop(proxy);
    });
    Ok(CodexSession { command,server:None,job:None,_launch_gate:None,stopped,socket:None,observer:Some(observer),mobile:Some(mobile),shared:Some(control),proxy_job,conversation })
}

#[cfg(test)]
#[path = "codex_shared_tests.rs"]
mod tests;

#[cfg(test)]
mod expected_interrupt_tests {
    use super::*;

    fn reply(ws: &mut WebSocket<TcpStream>, request: &Value, result: Value) {
        ws.send(Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).unwrap();
    }

    // 실제 소켓에서 초기화부터 닫기까지 교환하여 미지원 송신과 대상 바꾸기를 관측한다.
    fn close_over_socket(initialized: Value, reject_original: bool, disconnect_after_initialize: bool) -> (Result<(), String>, Vec<Value>) {
        let cwd = env::current_dir().unwrap().canonicalize().unwrap();
        let server_cwd = cwd.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut ws = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
            let mut received = Vec::new();
            loop {
                let request: Value = match ws.read() {
                    Ok(Message::Text(text)) => serde_json::from_str(&text).unwrap(),
                    Ok(_) => continue,
                    Err(_) => break,
                };
                assert!(received.len() < 6, "공유 닫기 요청을 재전송하면 안 된다");
                match request["method"].as_str().unwrap() {
                    "initialize" => {
                        reply(&mut ws, &request, initialized.clone());
                        if disconnect_after_initialize {
                            received.push(request.clone());
                            break;
                        }
                    },
                    "initialized" => {},
                    "thread/read" => reply(&mut ws, &request, json!({"thread":{"id":"selected-thread","cwd":server_cwd,"status":{"type":"active"}}})),
                    "thread/turns/list" => reply(&mut ws, &request, json!({"data":[{"id":"original-turn","status":"inProgress"}]})),
                    "turn/interrupt" => {
                        assert_eq!(request["params"], json!({"threadId":"selected-thread","turnId":"original-turn"}));
                        if reject_original {
                            // 원래 작업 종료 후 다른 작업이 시작된 서버의 명시 거절이다.
                            ws.send(Message::Text(json!({"id":request["id"],"error":{"code":-32600,"message":"original-turn completed; other-turn remains active"}}).to_string().into())).unwrap();
                        } else {
                            reply(&mut ws, &request, json!({}));
                        }
                    },
                    method => panic!("공유 닫기에서 예상하지 않은 요청: {method}"),
                }
                received.push(request);
            }
            received
        });
        let mut rpc = Rpc {ws:WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None),next_id:0,cancelled:None,deadline:Some(Instant::now()+Duration::from_secs(3))};
        let result = capture_supported_stop(&mut rpc, "selected-thread", &cwd)
            .and_then(|captured| interrupt_exact(&mut rpc, "selected-thread", &cwd, captured.as_deref()));
        drop(rpc);
        (result, worker.join().unwrap())
    }

    #[test]
    fn shared_close_requires_exact_true_on_each_sending_connection() {
        // true 다음 새 연결의 누락·false도 검사한다. 이전 연결이나 압축 지원은 근거가 아니다.
        let cases = [
            (json!({"supportsExpectedTurnInterrupt":true,"supportsCompactIfIdle":false}), true, false),
            (json!({"userAgent":"legacy-server"}), false, false),
            (json!({}), false, false),
            (json!({"supportsExpectedTurnInterrupt":false,"supportsCompactIfIdle":true}), false, false),
            (json!({"supportsCompactIfIdle":true}), false, false),
            (json!({"supportsExpectedTurnInterrupt":null}), false, true),
            (json!({"supportsExpectedTurnInterrupt":"true"}), false, true),
            (json!({"supportsExpectedTurnInterrupt":1}), false, true),
            (json!({"supportsExpectedTurnInterrupt":{}}), false, true),
            (json!({"supportsExpectedTurnInterrupt":[]}), false, true),
            (json!({"supportsExpectedTurnInterrupt":true,"supportsCompactIfIdle":true}), true, false),
            (json!({"supportsExpectedTurnInterrupt":false,"supportsCompactIfIdle":false}), false, false),
        ];
        for (response, supported, malformed) in cases {
            let (result, sent) = close_over_socket(response.clone(), false, false);
            assert_eq!(result.is_ok(), supported, "초기화 응답: {response}, 결과: {result:?}");
            assert_eq!(sent.iter().filter(|request| request["method"] == "turn/interrupt").count(), usize::from(supported), "초기화 응답: {response}");
            if !supported {
                let error = result.unwrap_err();
                assert!(error.contains("정확한 작업 취소"), "{error}");
                assert!(sent.iter().all(|request| matches!(request["method"].as_str(), Some("initialize" | "initialized"))), "미지원 연결에서 대화 제어를 보내면 안 된다: {sent:?}");
                if malformed { assert!(error.contains("지원 정보 형식"), "형식 진단을 보존해야 한다: {error}"); }
            }
        }
    }

    #[test]
    fn shared_close_preserves_exact_rejection_without_interrupting_another_turn() {
        let (result, sent) = close_over_socket(json!({"supportsExpectedTurnInterrupt":true}), true, false);
        assert!(result.unwrap_err().contains("other-turn remains active"));
        let methods: Vec<_> = sent.iter().map(|request| request["method"].as_str().unwrap()).collect();
        assert_eq!(methods, ["initialize", "initialized", "thread/read", "thread/turns/list", "turn/interrupt"]);
        assert_eq!(sent.iter().filter(|request| request["params"]["turnId"] == "other-turn").count(), 0);
    }

    #[test]
    fn shared_close_disconnect_cannot_authorize_the_next_connection() {
        let (disconnected, sent) = close_over_socket(json!({"supportsExpectedTurnInterrupt":true}), false, true);
        assert!(disconnected.is_err(), "단절 뒤 취소 접수를 확인했다고 보고하면 안 된다");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["method"], "initialize");
        let (unsupported, sent) = close_over_socket(json!({}), false, false);
        assert!(unsupported.unwrap_err().contains("정확한 작업 취소 지원"));
        assert_eq!(sent.iter().filter(|request| request["method"] == "turn/interrupt").count(), 0);
    }
}
