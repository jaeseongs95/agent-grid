//! 정상 종료와 완료 통지 사이의 경계를 실제 소유 PTY로 고정한다.
use super::*;

struct ReleaseCompletion(Option<mpsc::Sender<()>>);
impl ReleaseCompletion {
    fn release(&mut self) { if let Some(sender) = self.0.take() { let _ = sender.send(()); } }
}
impl Drop for ReleaseCompletion { fn drop(&mut self) { self.release(); } }

fn exited_with_pending_completion() -> (Engine, SessionInfo, ReleaseCompletion) {
    let (sender, receiver) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| {
        let final_event = matches!(&event, BackendEvent::Changed(info) if !info.active);
        sender.send(event).map_err(|error| error.to_string())?;
        if final_event { released.lock().unwrap().recv_timeout(Duration::from_secs(60)).map_err(|error| error.to_string())?; }
        Ok(())
    })).unwrap();
    let completion = ReleaseCompletion(Some(release));
    let cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
    let request = StartRequest { cwd, title: "완료 통지 보류 fixture".into(), project_id: None, project_assigned: false, thread_id: None, cols: 80, rows: 24 };
    let session = engine.start_fixture(request, shell("[Console]::WriteLine('resize-exit-ready')").unwrap()).unwrap();
    let mut output = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
            BackendEvent::Output(chunk) => {
                engine.acknowledge(&chunk.id, chunk.generation, chunk.seq).unwrap();
                let previous = output.matches("\x1b[6n").count();
                output.push_str(&chunk.data);
                for _ in previous..output.matches("\x1b[6n").count() { let _ = engine.write(&chunk.id, chunk.generation, "\x1b[1;1R"); }
            }
            BackendEvent::Changed(info) if !info.active => {
                assert_eq!(info.state, "exited");
                assert!(output.contains("resize-exit-ready"));
                eprintln!("판별 fixture: state={}, active={}, 완료 이벤트 전달 보류 중", info.state, info.active);
                return (engine, session, completion);
            }
            _ => {}
        }
    }
}

#[test]
fn resize_isolation_waits_for_normal_exit_completion_instead_of_five_ms() {
    let (engine, session, mut completion) = exited_with_pending_completion();
    assert!(engine.wait_for_exit(&session.id, session.generation, Duration::from_millis(5)).is_err());
    let (result, received) = mpsc::channel();
    let resizing = engine.clone(); let resized = session.clone();
    let worker = thread::spawn(move || result.send(resize_isolation_fixture(&resizing, &resized)).unwrap());
    let early = received.recv_timeout(Duration::from_millis(100));
    completion.release();
    let outcome = early.clone().unwrap_or_else(|_| received.recv_timeout(Duration::from_secs(2)).unwrap());
    worker.join().unwrap();
    assert!(early.is_err(), "정상 종료 중 5ms가 지났다는 이유만으로 실패하면 안 됩니다: {early:?}");
    outcome.unwrap();
    assert!(!engine.wait_for_exit(&session.id, session.generation, Duration::from_secs(2)).unwrap().active);
}

#[test]
fn resize_isolation_preserves_incomplete_cleanup_and_stale_generation_failures() {
    let (engine, session, mut completion) = exited_with_pending_completion();
    let mut stale = session.clone(); stale.generation += 1;
    assert!(resize_isolation_fixture(&engine, &stale).unwrap_err().contains("stale"));
    let started = Instant::now();
    let error = resize_isolation_fixture(&engine, &session).unwrap_err();
    eprintln!("미완료 경계: elapsed={:?}, error={error}", started.elapsed());
    assert!(error.contains("cleanup"), "실제 미완료의 원인을 보존해야 합니다: {error}");
    completion.release();
    assert!(!engine.wait_for_exit(&session.id, session.generation, Duration::from_secs(2)).unwrap().active);
}
