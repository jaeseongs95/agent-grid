use super::*;

fn request() -> StartRequest {
    StartRequest { cwd: std::env::current_dir().unwrap().to_string_lossy().into_owned(), title: "종료 복구 시험".into(), project_id: None, project_assigned: false, thread_id: None, cols: 80, rows: 24 }
}

fn engine_with_unknown() -> (Engine, Arc<Session>) {
    let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
    let session = engine.allocate(&request()).unwrap();
    session.cleanup_failed.store(true, Ordering::Release);
    session.finish("stopped").unwrap();
    (engine, session)
}

#[test]
fn explicit_local_exit_keeps_remote_unknown_and_closes_new_session_admission() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let (engine, session) = engine_with_unknown();
    assert!(engine.shutdown().is_err());
    assert!(engine.unconfirmed_exit_available().unwrap());
    assert!(!engine.inner.exiting.load(Ordering::Acquire), "조회는 종료 동의가 아니다");
    engine.shutdown_with_unconfirmed_remote().unwrap();
    assert_eq!(session.info().unwrap().state, "unknown");
    assert!(session.cleanup_failed.load(Ordering::Acquire));
    assert!(session.process.lock().unwrap().is_none());
    assert!(engine.allocate(&request()).is_err(), "명시 종료가 승리한 뒤 새 세션은 진입할 수 없다");
    assert!(engine.shutdown().is_err(), "명시 종료가 strict 중단 판정을 성공으로 바꾸면 안 된다");
    assert!(engine.inner.exiting.load(Ordering::Acquire));
}

#[test]
fn explicit_local_exit_rechecks_a_stale_offer_without_stopping_the_new_session() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let (engine, _) = engine_with_unknown();
    assert!(engine.unconfirmed_exit_available().unwrap());
    let newer = engine.allocate(&request()).unwrap();
    assert!(!engine.unconfirmed_exit_available().unwrap());
    assert!(engine.shutdown_with_unconfirmed_remote().is_err());
    assert!(!newer.stop_requested.load(Ordering::Acquire), "오래된 종료 선택지는 새 작업을 중단할 권한이 없다");
    assert_eq!(newer.phase.load(Ordering::Acquire), 0);
    assert!(!engine.inner.exiting.load(Ordering::Acquire));
    newer.finish("exited").unwrap();
    engine.shutdown_with_unconfirmed_remote().unwrap();
}

#[test]
fn explicit_local_exit_requires_a_completed_reaper_and_yields_to_management() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let (engine, session) = engine_with_unknown();
    *session.done.0.lock().unwrap() = false;
    assert!(!engine.unconfirmed_exit_available().unwrap());
    assert!(engine.shutdown_with_unconfirmed_remote().is_err());
    assert!(!engine.inner.exiting.load(Ordering::Acquire));
    *session.done.0.lock().unwrap() = true;
    let permit = codex::extension_admission().unwrap();
    assert!(engine.shutdown_with_unconfirmed_remote().is_err());
    assert!(!engine.inner.exiting.load(Ordering::Acquire));
    drop(permit);
    engine.shutdown_with_unconfirmed_remote().unwrap();
}

#[test]
fn explicit_local_exit_does_not_replace_normal_shutdown() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
    let session = engine.allocate(&request()).unwrap();
    session.finish("exited").unwrap();
    assert!(!engine.unconfirmed_exit_available().unwrap());
    assert!(engine.shutdown_with_unconfirmed_remote().is_err());
    assert!(!engine.inner.exiting.load(Ordering::Acquire));
    engine.shutdown().unwrap();
}
