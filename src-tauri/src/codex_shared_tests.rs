use super::*;
#[test]
fn shared_support_requires_declared_commands_and_never_treats_bad_help_as_legacy() {
    let options = "\nOptions:\n  -h, --help\n          Print help\n";
    let header = "Usage: fixture app-server [OPTIONS] [COMMAND]\n\n";
    assert!(supports_shared(format!("{header}Commands:\n  daemon  Manage daemon\n  proxy  Relay\n  help  Print help\n{options}").as_bytes()).unwrap());
    assert!(!supports_shared(format!("{header}Commands:\n  help  Print help\n        daemon proxy are description words\n{options}").as_bytes()).unwrap());
    assert!(!supports_shared(format!("Usage: fixture app-server [OPTIONS]\n{options}").as_bytes()).unwrap());
    assert!(supports_shared(format!("{header}{options}").as_bytes()).is_err());
    assert!(supports_shared(format!("사용법: codex app-server [OPTIONS] [COMMAND]\n\n명령어:\n  daemon  Manage daemon\n  proxy  Relay\n  help  Print help\n{options}").as_bytes()).unwrap());
    assert!(supports_shared(b"not a help document").is_err());
    assert!(supports_shared(&[0xff]).is_err());
}
#[test]
fn shared_resume_has_exact_id_and_no_websocket_token_option() {
    let cwd = env::current_dir().unwrap();
    let socket = cwd.join("fixture.sock");
    let endpoint = SharedEndpoint {exe:"fixture".into(),socket:socket.clone(),stamp:SocketStamp {canonical:socket,created:None,modified:None}};
    let command = endpoint.tui(&cwd, "exact-id").unwrap();
    let args: Vec<_> = command.get_argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(&args[1..3], ["resume", "exact-id"]);
    assert!(args.iter().any(|a| a.starts_with("unix://")));
    assert!(!args.iter().any(|a| a == "--remote-auth-token-env"));
}
#[test]
fn shared_lifecycle_and_turn_bounds_reject_ambiguous_targets() {
    assert!(lifecycle_socket(&json!({"status":"running","socketPath":"relative","appServerVersion":"1"})).is_err());
    assert!(lifecycle_socket(&json!({"status":"stopped"})).is_err());
    assert_eq!(active_turn(&json!({"data":[{"id":"turn-a","status":"inProgress"}]})).unwrap(), Some("turn-a"));
    assert_eq!(active_turn(&json!({"data":[{"id":"old","status":"completed"}]})).unwrap(), None);
    assert!(active_turn(&json!({"data":[{"status":"inProgress"}]})).is_err());
    assert!(active_turn(&json!({"data":[{},{}]})).is_err());
}
fn read_rpc(ws: &mut WebSocket<TcpStream>) -> Value {
    loop { if let Message::Text(text) = ws.read().unwrap() { return serde_json::from_str(&text).unwrap(); } }
}
fn reply(ws: &mut WebSocket<TcpStream>, request: &Value, result: Value) {
    ws.send(Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).unwrap();
}
#[test]
fn stop_pins_original_turn_before_tui_cleanup_and_never_retargets_mobile_turn() {
    let cwd = env::current_dir().unwrap().canonicalize().unwrap();
    let server_cwd = cwd.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut ws = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
        let read = read_rpc(&mut ws);
        assert_eq!(read["method"], "thread/read");
        assert_eq!(read["params"], json!({"threadId":"selected-thread","includeTurns":false}));
        reply(&mut ws, &read, json!({"thread":{"id":"selected-thread","cwd":server_cwd,"status":{"type":"active"}}}));
        let list = read_rpc(&mut ws);
        assert_eq!(list["method"], "thread/turns/list");
        assert_eq!(list["params"]["limit"], 1);
        assert_eq!(list["params"]["itemsView"], "summary");
        reply(&mut ws, &list, json!({"data":[{"id":"original-turn","status":"inProgress"}]}));
        // PTY 정리 중 모바일이 새 turn을 시작한 실패 경계다.
        let current_turn = "mobile-new-turn";
        let interrupt = read_rpc(&mut ws);
        assert_eq!(interrupt["method"], "turn/interrupt");
        assert_eq!(interrupt["params"], json!({"threadId":"selected-thread","turnId":"original-turn"}));
        assert_ne!(interrupt["params"]["turnId"], current_turn);
        ws.send(Message::Text(json!({"id":interrupt["id"],"error":{"code":-32600,"message":"expected active original-turn but found mobile-new-turn"}}).to_string().into())).unwrap();
        assert!(ws.read().is_err(), "새 turn 재조회·중단을 재시도하면 안 된다");
        assert_eq!(current_turn, "mobile-new-turn");
    });
    let mut rpc = Rpc {ws:WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None),next_id:0,cancelled:None,deadline:None};
    let captured = capture_turn(&mut rpc, "selected-thread", &cwd).unwrap();
    assert_eq!(captured.as_deref(), Some("original-turn"));
    assert!(interrupt_exact(&mut rpc, "selected-thread", &cwd, captured.as_deref()).unwrap_err().contains("mobile-new-turn"));
    drop(rpc);
    worker.join().unwrap();
}
#[test]
#[ignore = "격리 CODEX_HOME과 소유 시험 서버가 준비된 때만 명시적으로 실행한다"]
fn real_existing_backend_adapter_uses_one_backend_and_preserves_it() {
    let exe = env::var_os("OPENDIY_SHARED_FIXTURE_EXE").expect("격리 시험 실행 파일이 필요하다");
    let home = PathBuf::from(env::var_os("OPENDIY_SHARED_FIXTURE_HOME").expect("격리 시험 경로가 필요하다"));
    assert_eq!(env::var_os("CODEX_HOME"), Some(home.clone().into_os_string()));
    assert!(!home.join("auth.json").exists(), "사용자 자격증명을 읽으면 안 된다");
    let cwd = home.canonicalize().unwrap();
    let (sender, receiver) = mpsc::channel();
    let events = Arc::new(move |event| { sender.send(event).unwrap(); });
    let session = prepare_with_executable(&exe, &cwd, None, events).unwrap();
    assert!(session.server.is_none() && session.job.is_none(), "외부 서버를 소유하면 안 된다");
    assert!(session.shared.is_some());
    let id = match receiver.recv_timeout(Duration::from_secs(2)).unwrap() { CodexEvent::ThreadId(id) => id, event => panic!("새 대화 ID가 먼저 와야 한다: {event:?}") };
    let args: Vec<_> = session.command.get_argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(&args[1..3], ["resume", &id]);
    let mobile = session.mobile_connection();
    let reply = mobile.request(MobileAction::Status).unwrap();
    assert!(reply.shared && reply.mutations_allowed && !reply.isolated);
    assert_eq!(reply.status.unwrap().status, "disabled");
    let stop = session.shared_control().unwrap().capture_stop(Instant::now()+Duration::from_secs(3)).unwrap();
    stop.finish().unwrap();
    drop(session);
    assert!(mobile.request(MobileAction::Status).is_err(), "종료된 세션의 관리 핸들이 재연결하면 안 된다");
    let endpoint = SharedEndpoint::existing(&exe, &cwd, None).unwrap().expect("공유 서버가 유지되어야 한다");
    let mut proxy = endpoint.open(None).unwrap();
    initialize_proxy(&mut proxy).unwrap();
    let thread = proxy.call("thread/read", json!({"threadId":id,"includeTurns":false})).unwrap();
    assert_eq!(thread["thread"]["id"], id);
}

#[test]
#[ignore = "격리 CODEX_HOME에 공유 서버가 없는 상태에서만 명시적으로 실행한다"]
fn shared_capable_cli_without_backend_fails_without_an_owned_server_fallback() {
    let exe = env::var_os("OPENDIY_SHARED_FIXTURE_EXE").expect("격리 시험 실행 파일이 필요하다");
    let home = PathBuf::from(env::var_os("OPENDIY_SHARED_FIXTURE_HOME").expect("격리 시험 경로가 필요하다"));
    assert_eq!(env::var_os("CODEX_HOME"), Some(home.clone().into_os_string()));
    assert!(!home.join("auth.json").exists());
    let error = prepare_with_executable(&exe, &home, None, Arc::new(|_| {})).err().expect("공유 서버가 없으면 다른 서버로 대체하면 안 된다");
    assert!(error.contains("기존 공유 Codex 서버에 연결할 수 없습니다"), "{error}");
    assert!(error.contains("자동으로 설치하거나 시작하지 않았습니다"), "{error}");
}
