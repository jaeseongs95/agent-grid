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

#[test]
fn metadata_retention_preserves_the_last_unconfirmed_stop_in_memory_and_storage() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
    for _ in 0..MAX_METADATA_SESSIONS { engine.allocate(&request()).unwrap().finish("exited").unwrap(); }
    // 현재 퇴출 순서의 첫 기록만 불명으로 만들어 마지막 불명 유실을 결정적으로 재현한다.
    let unknown = engine.inner.sessions.lock().unwrap().values().next().unwrap().clone();
    unknown.cleanup_failed.store(true, Ordering::Release);
    unknown.finish("stopped").unwrap();
    let id = unknown.info().unwrap().id;
    let newer = engine.allocate(&request()).unwrap();
    newer.finish("exited").unwrap();
    assert!(engine.inner.sessions.lock().unwrap().contains_key(&id), "메타데이터 상한이 마지막 원격 중단 불명을 지우면 안 된다");
    assert_eq!(engine.inner.sessions.lock().unwrap().len(), MAX_METADATA_SESSIONS);
    let saved = engine.inner.store.lock().unwrap().sessions().unwrap();
    assert!(saved.iter().any(|session| session.id == id), "저장소 보존에서도 원격 중단 불명을 제외하면 안 된다");
    assert!(engine.shutdown().is_err(), "상한 정리가 명시 종료 동의를 대신하면 안 된다");
    assert!(engine.unconfirmed_exit_available().unwrap());
}

#[test]
fn unconfirmed_metadata_at_the_limit_refuses_new_sessions_without_eviction() {
    let _serial = lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
    for _ in 0..MAX_METADATA_SESSIONS {
        let session = engine.allocate(&request()).unwrap();
        session.cleanup_failed.store(true, Ordering::Release);
        session.finish("stopped").unwrap();
    }
    assert!(engine.allocate(&request()).is_err(), "불명만으로 상한이 찬 경우 새 세션을 거절해야 한다");
    let registry = engine.inner.sessions.lock().unwrap();
    assert_eq!(registry.len(), MAX_METADATA_SESSIONS);
    assert!(registry.values().all(|session| session.cleanup_failed.load(Ordering::Acquire)));
}
