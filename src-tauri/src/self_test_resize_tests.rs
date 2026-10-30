//! 정상 종료와 완료 통지 사이의 경계를 실제 소유 PTY로 고정한다.
use super::*;

struct ReleaseCompletion(Option<mpsc::Sender<()>>);
impl ReleaseCompletion {
    fn release(&mut self) { if let Some(sender) = self.0.take() { let _ = sender.send(()); } }
}
impl Drop for ReleaseCompletion { fn drop(&mut self) { self.release(); } }

struct ResizeFixtureStages {
    root: PathBuf,
    pending: Option<(Engine, SessionInfo, mpsc::Sender<()>)>,
}
impl ResizeFixtureStages {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("opendiy-resize-stage-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap_or_else(|error| panic!("전용 단계 디렉터리 생성 실패: {:?}", error.kind()));
        Self { root, pending: None }
    }

    fn script(&self) -> String {
        let root = self.root.to_str().expect("전용 단계 경로를 문자열로 표현할 수 없습니다").replace('\'', "''");
        // 비콘 게시 실패는 정상 미도달과 구분하며 예외 원문·경로를 출력하지 않는다.
        format!("try {{ $stage='entered'; [IO.File]::WriteAllText('{root}/entered',''); $stage='console-marker'; [Console]::WriteLine('resize-exit-ready'); $stage='marker-returned'; [IO.File]::WriteAllText('{root}/marker-returned',''); $stage='exit-intent'; [IO.File]::WriteAllText('{root}/exit-intent','') }} catch {{ [Console]::Error.WriteLine('resize-fixture-stage-error:'+$stage); exit 21 }}")
    }

    fn log(&self, boundary: &str, output: &str) {
        let stages = ["entered", "marker-returned", "exit-intent"].map(|stage| {
            let publish_failed = output.contains(&format!("resize-fixture-stage-error:{stage}"));
            let observed = if publish_failed { Ok("게시실패관측") } else { match std::fs::metadata(self.root.join(stage)) {
                Ok(metadata) if metadata.is_file() => Ok("게시확인"),
                Ok(_) => Err(std::io::ErrorKind::InvalidData),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("미게시·원인미확인"),
                Err(error) => Err(error.kind()),
            } };
            (stage, observed)
        });
        let errors = ["owned launch gate", "open owned console", "launch owned child:", "reap owned child:", "resize-fixture-stage-error:entered", "resize-fixture-stage-error:console-marker", "resize-fixture-stage-error:marker-returned", "resize-fixture-stage-error:exit-intent"]
            .map(|class| (class, output.contains(class)));
        // 단계는 스크립트 관측이며 실제 자식 종료나 입력 응답 소비를 뜻하지 않는다.
        eprintln!("단계 판별: 경계={boundary}, 비콘={stages:?}, 제어질의={:?}, 오류분류={errors:?}, 실제자식종료코드=미관측", resize_fixture_control_counts(output));
    }
}
impl Drop for ResizeFixtureStages {
    fn drop(&mut self) {
        if let Some((engine, session, release)) = self.pending.take() {
            // 실패 경로에서만, 자신의 완료 sink를 먼저 해제한 뒤 정확한 소유 세대를 정리한다.
            let released = release.send(()).is_ok();
            let stopped = if engine.stop(&session.id, session.generation).is_ok() { "성공" } else { "실패" };
            let completed = engine.wait_for_exit(&session.id, session.generation, Duration::ZERO).is_ok();
            let completion = if completed { "성공" } else { "미확인" };
            eprintln!("준비 실패 자원 정리: 보류해제통지성공={released}, 중단반환={stopped}, 완료확인={completion}");
            if !completed {
                eprintln!("전용 비콘 정리 보류: 소유 자식의 완료가 미확인이라 단계 파일을 유지합니다");
                return;
            }
        }
        // 이 guard가 생성한 세 파일과 빈 디렉터리만 정리한다. 재귀 삭제는 하지 않는다.
        for stage in ["entered", "marker-returned", "exit-intent"] {
            if let Err(error) = std::fs::remove_file(self.root.join(stage)) {
                if error.kind() != std::io::ErrorKind::NotFound { eprintln!("전용 단계 파일 정리 실패: 단계={stage}, 오류종류={:?}", error.kind()); }
            }
        }
        if let Err(error) = std::fs::remove_dir(&self.root) { eprintln!("전용 단계 디렉터리 정리 실패: 오류종류={:?}", error.kind()); }
    }
}

fn resize_fixture_control_counts(output: &str) -> [(&'static str, usize); 7] {
    [
        ("cursor", output.matches("\x1b[6n").count()),
        ("status", output.matches("\x1b[5n").count()),
        ("private-cursor", output.matches("\x1b[?6n").count()),
        ("device-primary", output.matches("\x1b[c").count() + output.matches("\x1b[0c").count()),
        ("device-secondary", output.matches("\x1b[>c").count() + output.matches("\x1b[>0c").count()),
        ("color-foreground", output.matches("\x1b]10;?").count()),
        ("color-background", output.matches("\x1b]11;?").count()),
    ]
}

fn exited_with_pending_completion() -> (Engine, SessionInfo, ReleaseCompletion) {
    let mut stages = ResizeFixtureStages::new();
    let (sender, receiver) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| {
        let final_event = matches!(&event, BackendEvent::Changed(info) if !info.active);
        sender.send(event).map_err(|error| error.to_string())?;
        if final_event { released.lock().unwrap().recv_timeout(Duration::from_secs(60)).map_err(|error| error.to_string())?; }
        Ok(())
    })).unwrap();
    let release_on_failure = release.clone();
    let completion = ReleaseCompletion(Some(release));
    let cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
    let request = StartRequest { cwd, title: "완료 통지 보류 fixture".into(), project_id: None, project_assigned: false, thread_id: None, cols: 80, rows: 24 };
    let launching = Instant::now();
    let session = engine.start_fixture(request, shell(&stages.script()).unwrap()).unwrap();
    stages.pending = Some((engine.clone(), session.clone(), release_on_failure));
    let launch_ms = launching.elapsed().as_millis();
    let mut output = String::new();
    let mut frames = 0;
    let mut cursor_reply_errors = 0;
    let mut last_state = (session.state.clone(), session.active);
    let mut first_output_ms = None;
    let mut last_output_ms = None;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    loop {
        let event = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_else(|error| {
            stages.log("수신 실패", &output);
            eprintln!("출력 시각: 시작반환ms={launch_ms}, 최초수신ms={first_output_ms:?}, 최종수신ms={last_output_ms:?}");
            // 출력 원문·경로·환경은 기록하지 않고 이 전용 fixture의 진행 단계만 남긴다.
            let marker = output.contains("resize-exit-ready");
            let stage = if marker { "표식 수신 뒤 종료·정리 대기" } else if frames > 0 { "출력 수신 뒤 표식 대기" } else { "첫 출력 대기" };
            let current = engine.list().map(|infos| infos.into_iter()
                .find(|info| info.id == session.id && info.generation == session.generation)
                .map(|info| (info.state, info.active)));
            let completion_state = engine.wait_for_exit(&session.id, session.generation, Duration::ZERO)
                .map(|info| (info.state, info.active));
            panic!("종료 준비 실패: 단계={stage}, 수신오류={error:?}, 경과={:?}, 표식={marker}, 출력프레임={frames}, 출력바이트={}, 직전상태={last_state:?}, 엔진상태={current:?}, 완료확인={completion_state:?}, 커서질의={}, 커서응답실패={cursor_reply_errors}", started.elapsed(), output.len(), output.matches("\x1b[6n").count());
        });
        match event {
            BackendEvent::Output(chunk) => {
                let now_ms = launching.elapsed().as_millis();
                first_output_ms.get_or_insert(now_ms);
                last_output_ms = Some(now_ms);
                frames += 1;
                engine.acknowledge(&chunk.id, chunk.generation, chunk.seq).unwrap();
                let previous = resize_fixture_control_counts(&output);
                output.push_str(&chunk.data);
                let controls = resize_fixture_control_counts(&output);
                for ((class, count), (_, before)) in controls.into_iter().zip(previous) {
                    if count > before { eprintln!("제어질의 수신: 종류={class}, 누적={count}, 수신ms={now_ms}"); }
                }
                for _ in previous[0].1..controls[0].1 {
                    eprintln!("커서 응답 쓰기 시작: 경과ms={}", launching.elapsed().as_millis());
                    let written = engine.write(&chunk.id, chunk.generation, "\x1b[1;1R");
                    eprintln!("커서 응답 쓰기 반환: 성공={}, 경과ms={}, 상대소비=미관측", written.is_ok(), launching.elapsed().as_millis());
                    if written.is_err() { cursor_reply_errors += 1; }
                }
            }
            BackendEvent::Changed(info) if !info.active => {
                stages.log("종료 이벤트", &output);
                eprintln!("출력 시각: 시작반환ms={launch_ms}, 최초수신ms={first_output_ms:?}, 최종수신ms={last_output_ms:?}");
                // 정상 종료가 아닌 경우에도 표식 도달 여부와 정리 직전 관측을 보존한다.
                eprintln!("종료 이벤트 판별: 상태={}, 활성={}, 경과={:?}, 표식={}, 출력프레임={frames}, 출력바이트={}, 직전상태={last_state:?}, 커서질의={}, 커서응답실패={cursor_reply_errors}, 완료 이벤트 전달 보류 중", info.state, info.active, started.elapsed(), output.contains("resize-exit-ready"), output.len(), output.matches("\x1b[6n").count());
                assert_eq!(info.state, "exited");
                assert!(output.contains("resize-exit-ready"));
                stages.pending = None; // 성공 시에는 기존 시험의 완료 보류를 그대로 넘긴다.
                return (engine, session, completion);
            }
            BackendEvent::Changed(info) => { last_state = (info.state, info.active); }
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
