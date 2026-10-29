use super::*;

fn status_value() -> Value { json!({"status":"connected","serverName":"fixture","environmentId":"env-fixture","installationId":"not-exported"}) }
fn fixture_connection(endpoint: String) -> MobileConnection {
    MobileConnection(Arc::new(Management {endpoint,token:"fixture-private-capability".into(),job:Weak::new(),stopped:Arc::new(AtomicBool::new(false)),admission:Mutex::new(()),socket:Mutex::new(None),namespaced_remote:true}))
}

#[cfg(windows)]
#[test]
fn owned_mobile_server_fixture_child() {
    let Some(ready) = env::var_os("OPENDIY_MOBILE_FIXTURE_READY") else { return; };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(ready, listener.local_addr().unwrap().to_string()).unwrap();
    let (stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut ws = tungstenite::accept_hdr(stream, |request: &tungstenite::handshake::server::Request, response| {
        assert_eq!(request.headers()["Authorization"], "Bearer fixture-private-capability");
        Ok(response)
    }).unwrap();
    initialize_fixture(&mut ws);
    let status = read(&mut ws);
    assert_eq!(status["method"], "remoteControl/status/read");
    reply(&mut ws, &status, status_value());
    assert!(ws.read().is_err());
    // Stay alive until the parent's last Job handle is closed.
    loop { thread::sleep(Duration::from_millis(100)); }
}

#[cfg(windows)]
#[test]
fn real_owned_mobile_connection_pins_peer_and_does_not_keep_the_server_job_alive() {
    let root = env::temp_dir().join(format!("opendiy-mobile-owned-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let ready = root.join("ready");
    let (mut command, gate) = LaunchGate::wrap_std(env::current_exe().unwrap().as_os_str()).unwrap();
    command.args(["--exact", "codex::mobile::tests::owned_mobile_server_fixture_child", "--nocapture"])
        .env("OPENDIY_MOBILE_FIXTURE_READY", &ready).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    hidden(&mut command);
    let child = command.spawn().unwrap();
    struct OwnedFixture { child: Child, job: Option<Arc<ProcessJob>>, _gate: LaunchGate }
    impl Drop for OwnedFixture {
        fn drop(&mut self) {
            self.job.take();
            let _ = self.child.kill(); let _ = self.child.wait();
        }
    }
    let mut owned = OwnedFixture { job:Some(Arc::new(ProcessJob::assign_std(&child).unwrap())), child, _gate:gate };
    owned._gate.release().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let address = loop {
        if let Ok(value) = std::fs::read_to_string(&ready) {
            if value.parse::<std::net::SocketAddr>().is_ok() { break value; }
        }
        assert!(owned.child.try_wait().unwrap().is_none(), "owned mobile fixture exited before listening");
        assert!(Instant::now() < deadline, "owned mobile fixture did not start");
        thread::sleep(Duration::from_millis(20));
    };
    let endpoint = format!("ws://{address}");
    let connection = MobileConnection::new(endpoint.clone(), "fixture-private-capability".into(), owned.job.as_ref().unwrap(), Arc::new(AtomicBool::new(false)), true);
    let result = connection.request(MobileAction::Status).unwrap();
    assert_eq!(result.status.unwrap().status, "connected");
    assert_eq!(Arc::strong_count(owned.job.as_ref().unwrap()), 1, "management must not own the server Job");
    let mut clone = connection.clone();
    owned.job.take(); // KILL_ON_JOB_CLOSE must work while management clones still exist.
    owned.child.wait().unwrap();
    assert!(clone.0.job.upgrade().is_none());
    assert!(clone.request(MobileAction::Status).is_err());

    // A stale handle must send no bearer bytes to an unrelated listener, even
    // if its address were replaced. A fresh fixture port avoids Windows'
    // TIME_WAIT rebind delay; the production endpoint remains immutable.
    drop(connection);
    let replacement = TcpListener::bind("127.0.0.1:0").unwrap();
    Arc::get_mut(&mut clone.0).unwrap().endpoint = format!("ws://{}", replacement.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let (mut stream, _) = replacement.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut bytes = [0u8; 1];
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
    });
    assert!(clone.request(MobileAction::Status).err().unwrap().contains("종료"));
    worker.join().unwrap();
    drop(owned);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_cli_mobile_mutations_are_rejected_before_connecting() {
    let mut connection = fixture_connection("ws://127.0.0.1:1".into());
    Arc::get_mut(&mut connection.0).unwrap().namespaced_remote = false;
    for action in [MobileAction::Enable, MobileAction::Disable, MobileAction::Pair] {
        assert!(connection.request(action).err().unwrap().contains("--remote-control-client-name"));
        assert!(connection.0.socket.lock().unwrap().is_none());
    }
    assert!(!connection.request(MobileAction::Status).err().unwrap().contains("--remote-control-client-name"));
}
fn fixture(operation: impl FnOnce(WebSocket<TcpStream>) + Send + 'static) -> (MobileConnection, TcpStream, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let ws = tungstenite::accept_hdr(stream, |request: &tungstenite::handshake::server::Request, response| {
            assert_eq!(request.headers()["Authorization"], "Bearer fixture-private-capability");
            Ok(response)
        }).unwrap();
        operation(ws);
    });
    (fixture_connection(format!("ws://{address}")), TcpStream::connect(address).unwrap(), worker)
}
fn read(ws: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = ws.read().unwrap() { return serde_json::from_str(&text).unwrap(); }
    }
}
fn reply(ws: &mut WebSocket<TcpStream>, request: &Value, result: Value) {
    ws.send(Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).unwrap();
}
fn initialize_fixture(ws: &mut WebSocket<TcpStream>) {
    let init = read(ws);
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
    reply(ws, &init, json!({}));
    assert_eq!(read(ws)["method"], "initialized");
}

#[test]
fn four_actions_are_closed_and_mutations_are_ephemeral() {
    for action in [MobileAction::Status,MobileAction::Enable,MobileAction::Disable,MobileAction::Pair] {
        let (connection, stream, worker) = fixture(move |mut ws| {
            initialize_fixture(&mut ws);
            let status = read(&mut ws);
            assert_eq!(status["method"], "remoteControl/status/read");
            reply(&mut ws, &status, status_value());
            if !matches!(action, MobileAction::Status) {
                let request = read(&mut ws);
                let result = match action {
                    MobileAction::Enable => {assert_eq!(request["method"],"remoteControl/enable");assert_eq!(request["params"],json!({"ephemeral":true}));status_value()},
                    MobileAction::Disable => {assert_eq!(request["method"],"remoteControl/disable");assert_eq!(request["params"],json!({"ephemeral":true}));status_value()},
                    MobileAction::Pair => {assert_eq!(request["method"],"remoteControl/pairing/start");assert_eq!(request["params"],json!({"manualCode":true}));json!({"pairingCode":"machine-code-must-not-escape","manualPairingCode":"ABCD-1234","environmentId":"env-fixture","expiresAt":2000000000})},
                    MobileAction::Status => unreachable!(),
                };
                reply(&mut ws,&request,result);
            }
            assert!(ws.read().is_err()); // Every success closes its one management connection.
        });
        let result = connection.run_stream(action,stream,Duration::from_secs(2)).unwrap();
        assert_eq!(result.status.is_none(),matches!(action,MobileAction::Pair));
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains("machine-code"));
        assert!(!encoded.contains("installationId"));
        assert!(!encoded.contains("fixture-private-capability"));
        assert!(connection.0.socket.lock().unwrap().is_none());
        worker.join().unwrap();
    }
    for invalid in ["stop","start","remoteControl/enable","restart",""] { assert!(serde_json::from_value::<MobileAction>(json!(invalid)).is_err()); }
}

#[test]
fn errors_never_echo_server_secrets_and_never_continue_to_a_mutation() {
    for code in [-32601,-32000] {
        let (connection,stream,worker) = fixture(move |mut ws| {
            initialize_fixture(&mut ws);
            let request=read(&mut ws);
            ws.send(Message::Text(json!({"id":request["id"],"error":{"code":code,"message":"Bearer fixture-private-capability machine-code","data":{"secret":"body-secret"}}}).to_string().into())).unwrap();
            assert!(ws.read().is_err());
        });
        let error=connection.run_stream(MobileAction::Enable,stream,Duration::from_secs(2)).err().unwrap();
        assert!(!error.contains("Bearer"));assert!(!error.contains("private-capability"));assert!(!error.contains("body-secret"));
        assert!(error.contains(if code == -32601 {"지원하지"} else {"ChatGPT"}));
        assert!(connection.0.socket.lock().unwrap().is_none());worker.join().unwrap();
    }
}

#[test]
fn cancellation_closes_pending_read_and_clones_share_admission() {
    let (ready_tx,ready_rx)=mpsc::channel();
    let (connection,stream,worker)=fixture(move |mut ws| {
        initialize_fixture(&mut ws);let _request=read(&mut ws);ready_tx.send(()).unwrap();
        assert!(ws.read().is_err());
    });
    let clone=connection.clone();
    let request=thread::spawn(move || clone.run_stream(MobileAction::Status,stream,Duration::from_secs(2)));
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();connection.cancel();
    assert!(request.join().unwrap().is_err());
    assert!(connection.0.socket.lock().unwrap().is_none());worker.join().unwrap();
    assert!(connection.request(MobileAction::Enable).is_err());
    let fresh=fixture_connection("ws://127.0.0.1:1".into());
    let _guard=fresh.0.admission.lock().unwrap();
    assert!(fresh.clone().request(MobileAction::Status).err().unwrap().contains("진행 중"));
}

#[test]
fn timeout_and_oversized_response_close_the_socket() {
    for oversized in [false,true] {
        let (connection,stream,worker)=fixture(move |mut ws| {
            initialize_fixture(&mut ws);let request=read(&mut ws);
            if oversized { let _=ws.send(Message::Text(json!({"id":request["id"],"result":{"secret":"x".repeat(MAX_REPLY+1)}}).to_string().into())); }
            assert!(ws.read().is_err());
        });
        let error=connection.run_stream(MobileAction::Status,stream,Duration::from_millis(30)).err().unwrap();
        assert!(!error.contains("secret"));assert!(!error.contains("private-capability"));
        if !oversized { assert!(error.contains("초과")); }
        assert!(connection.0.socket.lock().unwrap().is_none());worker.join().unwrap();
    }
}

#[test]
fn response_shape_bounds_and_ownership_preflight_are_fail_closed() {
    assert!(parse_status(&status_value(),"private").is_ok());
    for value in [json!({}),json!({"status":"unknown","serverName":"ok","environmentId":null}),json!({"status":"connected","serverName":"x".repeat(257),"environmentId":null}),json!({"status":"connected","serverName":"Bearer private","environmentId":null})] { assert!(parse_status(&value,"private").is_err()); }
    for value in [json!({"manualPairingCode":null}),json!({"manualPairingCode":"bad\ncode","expiresAt":1,"environmentId":"id"}),json!({"manualPairingCode":"ok","expiresAt":0,"environmentId":"id"})] { assert!(parse_pairing(&value,"private").is_err()); }
    for endpoint in ["ws://example.com:80","ws://192.168.1.1:80","wss://127.0.0.1:80","ws://127.0.0.1:0","ws://127.0.0.1:80/path"] { assert!(owned_address(endpoint).is_err()); }
    assert!(owned_address("ws://127.0.0.1:80").is_ok());
}
