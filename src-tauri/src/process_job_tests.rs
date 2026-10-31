//! Windows 실제 Job 경계 시험. 별도 앱 시험 프로세스와 자체 fixture만 사용한다.
//! 먼저 `cargo build --manifest-path src-tauri/Cargo.toml --bin opendiy-desktop-helper`가 필요하다.
use super::{install_app_job, LaunchGate, ProcessJob};
use std::{
    fs,
    io::{Read, Write},
    os::windows::{io::{AsRawHandle, FromRawHandle, OwnedHandle}, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{GetExitCodeProcess, OpenProcess, TerminateProcess, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE},
};

const FIXTURE: &str = "process_job::tests::job_fixture";
const TIMEOUT: Duration = Duration::from_secs(10);

/// 시험이 직접 생성한 프로세스만 열고, 실패 경로에서도 정리한다.
struct FixtureProcess { handle: OwnedHandle, cleanup: bool }
impl FixtureProcess {
    fn open(pid: u32) -> Self {
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(!raw.is_null(), "시험 프로세스 {pid} 열기: {}", std::io::Error::last_os_error());
        Self { handle: unsafe { OwnedHandle::from_raw_handle(raw) }, cleanup: true }
    }
    fn running(&self) -> bool {
        let result = unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) };
        assert!(matches!(result, WAIT_OBJECT_0 | WAIT_TIMEOUT), "시험 프로세스 대기 실패: {}", std::io::Error::last_os_error());
        result == WAIT_TIMEOUT
    }
    fn await_exit(&self) {
        assert_eq!(unsafe { WaitForSingleObject(self.handle.as_raw_handle(), TIMEOUT.as_millis() as u32) }, WAIT_OBJECT_0, "시험 프로세스가 종료되지 않았습니다.");
    }
    fn exit_code(&self) -> u32 {
        let mut code = 0;
        assert_ne!(unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) }, 0, "시험 프로세스 종료 코드 조회 실패: {}", std::io::Error::last_os_error());
        code
    }
    fn hand_off(mut self) { self.cleanup = false; }
}
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if self.cleanup {
            // 이미 종료된 경우도 안전하다. 핸들은 시험에서 얻은 동일 실행 세대를 가리킨다.
            unsafe { TerminateProcess(self.handle.as_raw_handle(), 1); WaitForSingleObject(self.handle.as_raw_handle(), 10_000); }
        }
    }
}

struct TestDirectory(PathBuf);
impl Drop for TestDirectory {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

fn fixture_args() -> [&'static str; 4] { ["--ignored", "--exact", FIXTURE, "--nocapture"] }
fn configure(command: &mut Command, directory: &Path, role: &str) {
    command.args(fixture_args()).env("OPENDIY_JOB_TEST_DIRECTORY", directory).env("OPENDIY_JOB_TEST_ROLE", role)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    if matches!(role, "query" | "daemon") {
        command.stderr(fs::File::create(directory.join(format!("{role}-stderr"))).unwrap());
    }
}
fn fixture_command(directory: &Path, role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    configure(&mut command, directory, role);
    command
}
fn publish(directory: &Path, name: &str, value: &impl serde::Serialize) {
    let path = directory.join(name);
    let temporary = path.with_extension("writing");
    fs::write(&temporary, serde_json::to_vec(value).unwrap()).unwrap();
    fs::rename(temporary, path).unwrap();
}
fn stage(directory: &Path, role: &str, step: &str) {
    fs::write(directory.join(format!("{role}-stage")), step).unwrap();
    eprintln!("Job 시험 단계: 역할={role}, 단계={step}");
}
fn diagnostics(directory: &Path) -> String {
    let mut records = Vec::new();
    for name in ["app-stage", "pty-stage", "query-stage", "daemon-stage", "pty-output", "pty-pump", "query-stderr", "daemon-stderr", "app-failure", "pty-failure", "query-failure", "daemon-failure"] {
        if let Ok(file) = fs::File::open(directory.join(name)) {
            let mut bytes = Vec::new();
            let result = file.take(4096).read_to_end(&mut bytes);
            records.push(format!("{name}={:?}, 읽기={result:?}", String::from_utf8_lossy(&bytes)));
        }
    }
    records.join("; ")
}
fn receive<T: serde::de::DeserializeOwned>(directory: &Path, name: &str, watched: Option<&FixtureProcess>) -> T {
    let started = Instant::now();
    loop {
        match fs::read(directory.join(name)) {
            Ok(bytes) => return serde_json::from_slice(&bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(process) = watched {
                    assert!(process.running(), "시험 응답 {name} 이전 자식 종료: 코드={}, {}", process.exit_code(), diagnostics(directory));
                }
                assert!(started.elapsed() < TIMEOUT, "시험 응답 {name} 시간 초과: {}", diagnostics(directory));
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("시험 응답 {name} 읽기: {error}"),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SpawnReport { detached: Option<u32>, blocked: Option<i32>, ordinary: u32 }

fn check_launcher(job: ProcessJob, helper: &FixtureProcess, directory: &Path, role: &str) -> Option<FixtureProcess> {
    stage(directory, role, "자손 보고 대기");
    let report: SpawnReport = receive(directory, role, Some(helper));
    // 판정 전에 성공한 모든 fixture를 소유해 예상 밖 분리도 정리한다.
    let detached = report.detached.map(FixtureProcess::open);
    let ordinary = FixtureProcess::open(report.ordinary);
    assert!(ordinary.running());
    if role == "daemon" {
        assert!(detached.as_ref().is_some_and(FixtureProcess::running), "전용 실행기의 명시 분리가 거절됐습니다. 외부 Job 제한도 확인하십시오: {:?}", report.blocked);
        assert!(report.blocked.is_none());
    } else {
        assert!(detached.is_none(), "일반 {role} 실행기에서 분리를 허용했습니다.");
        assert_eq!(report.blocked, Some(ERROR_ACCESS_DENIED as i32));
    }
    stage(directory, role, "분리 판정 완료, Job 닫기");
    drop(job);
    helper.await_exit();
    ordinary.await_exit();
    if let Some(process) = &detached { assert!(process.running(), "전용 실행기 Job 종료가 daemon을 종료했습니다."); }
    stage(directory, role, "일반 자손 회수 확인");
    detached
}

fn app_fixture(directory: &Path) {
    // 전체 시험 실행기를 APP_JOB에 넣지 않도록 이 fixture 프로세스에서만 설치한다.
    install_app_job().unwrap();
    stage(directory, "app", "앱 Job 설치 완료, PTY 검사 시작");
    pty_fixture(directory);
    let executable = std::env::current_exe().unwrap();
    for role in ["query", "daemon"] {
        stage(directory, "app", &format!("{role} 실행기 생성"));
        let (mut command, gate) = LaunchGate::wrap_std(executable.as_os_str()).unwrap();
        configure(&mut command, directory, role);
        let mut child = command.spawn().unwrap();
        let helper = FixtureProcess::open(child.id());
        let job = if role == "daemon" { ProcessJob::assign_daemon_launcher(&child) } else { ProcessJob::assign_std(&child) }.unwrap();
        gate.release().unwrap();
        stage(directory, role, "실행 게이트 해제");
        let detached = check_launcher(job, &helper, directory, role);
        child.wait().unwrap();
        if let Some(detached) = detached {
            // launcher Job이 닫힌 뒤에도 앱 Job만 상속한 일반 자식은 살아 있다.
            let child = fixture_command(directory, "sleeper").spawn().unwrap();
            let ordinary = FixtureProcess::open(child.id());
            assert!(ordinary.running());
            let report: SpawnReport = receive(directory, "daemon", None);
            stage(directory, "app", "daemon 분리 확인, 앱 종료 지시 대기");
            publish(directory, "app-ready", &(report.detached.unwrap(), child.id()));
            let _: bool = receive(directory, "finish-app", None);
            detached.hand_off();
            ordinary.hand_off();
        }
    }
}

fn pty_fixture(directory: &Path) {
    stage(directory, "pty", "가상 터미널 생성");
    let pair = portable_pty::native_pty_system().openpty(portable_pty::PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let output_directory = directory.to_path_buf();
    let output = std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
        let mut captured = fs::File::create(output_directory.join("pty-output")).map_err(|error| error.to_string())?;
        let mut buffer = [0u8; 4096];
        let (mut retained, mut queries) = (0usize, 0usize);
        let mut tail = Vec::new();
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => break,
                Err(error) => return Err(format!("PTY 출력 읽기: {error}")),
            };
            let keep = count.min((16 * 1024usize).saturating_sub(retained));
            captured.write_all(&buffer[..keep]).map_err(|error| error.to_string())?;
            captured.flush().map_err(|error| error.to_string())?;
            retained += keep;
            for byte in &buffer[..count] {
                tail.push(*byte);
                if tail.len() > 4 { tail.remove(0); }
                // ConPTY의 커서 상속 질의는 분할 수신될 수 있다. 사용자 입력은 보내지 않는다.
                if tail.as_slice() == b"\x1b[6n" {
                    writer.write_all(b"\x1b[1;1R").and_then(|_| writer.flush()).map_err(|error| format!("PTY 커서 응답: {error}"))?;
                    queries += 1;
                    fs::write(output_directory.join("pty-pump"), format!("커서 응답={queries}, 보관 바이트={retained}")).map_err(|error| error.to_string())?;
                }
            }
        }
        fs::write(output_directory.join("pty-pump"), format!("출력 종료, 커서 응답={queries}, 보관 바이트={retained}")).map_err(|error| error.to_string())?;
        Ok(())
        })();
        if let Err(error) = &result { let _ = fs::write(output_directory.join("pty-pump"), error); }
        result
    });
    stage(directory, "pty", "출력 소비 시작, 실행기 생성");
    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
    command.args(fixture_args());
    command.env("OPENDIY_JOB_TEST_DIRECTORY", directory);
    command.env("OPENDIY_JOB_TEST_ROLE", "pty");
    let (command, gate) = LaunchGate::wrap_pty(command).unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    stage(directory, "pty", "실행기 생성 완료, Job 배정");
    let helper = FixtureProcess::open(child.process_id().unwrap());
    let job = ProcessJob::assign_pty(child.as_ref()).unwrap();
    gate.release().unwrap();
    stage(directory, "pty", "실행 게이트 해제");
    assert!(check_launcher(job, &helper, directory, "pty").is_none());
    child.wait().unwrap();
    stage(directory, "pty", "실행기 회수 완료, 가상 터미널 닫기");
    drop(pair);
    stage(directory, "pty", "가상 터미널 닫힘, 출력 소비 완료 대기");
    output.join().unwrap().unwrap();
    eprintln!("Job 시험 터미널 협상: {}", fs::read_to_string(directory.join("pty-pump")).unwrap());
    stage(directory, "pty", "출력 소비 완료");
}

#[test]
fn only_daemon_launcher_can_outlive_launcher_and_app_jobs() {
    let directory = TestDirectory(std::env::temp_dir().join(format!("opendiy-job-test-{}", uuid::Uuid::new_v4())));
    fs::create_dir(&directory.0).unwrap();
    let mut app = fixture_command(&directory.0, "app").stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap();
    let app_process = FixtureProcess::open(app.id());
    let (daemon_pid, ordinary_pid): (u32, u32) = receive(&directory.0, "app-ready", Some(&app_process));
    let daemon = FixtureProcess::open(daemon_pid);
    let ordinary = FixtureProcess::open(ordinary_pid);
    assert!(daemon.running());
    assert!(ordinary.running());
    publish(&directory.0, "finish-app", &true);
    app_process.await_exit();
    assert!(app.wait().unwrap().success(), "격리된 앱 Job 시험이 실패했습니다.");
    ordinary.await_exit();
    assert!(daemon.running(), "앱 종료가 분리된 daemon을 종료했습니다.");
}

#[test]
#[ignore = "자체 시험 프로세스의 진입점"]
fn job_fixture() {
    let Ok(directory) = std::env::var("OPENDIY_JOB_TEST_DIRECTORY") else { return; };
    let directory = Path::new(&directory);
    let role = std::env::var("OPENDIY_JOB_TEST_ROLE").unwrap();
    let failure_path = directory.join(format!("{role}-failure"));
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |information| {
        let _ = fs::write(&failure_path, information.to_string());
        previous_hook(information);
    }));
    if role != "sleeper" { stage(directory, &role, "자체 시험 진입"); }
    if role == "app" {
        app_fixture(directory);
    } else if role == "sleeper" {
        // 시험 도중 부모가 강제 종료돼도 fixture는 무한히 남지 않는다.
        std::thread::sleep(Duration::from_secs(30));
    } else {
        assert!(matches!(role.as_str(), "query" | "pty" | "daemon"));
        let mut detached_command = fixture_command(directory, "sleeper");
        detached_command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        stage(directory, &role, "명시 분리 자식 생성");
        let detached = detached_command.spawn();
        let (detached, blocked) = match detached {
            Ok(child) => (Some(child), None),
            Err(error) => (None, error.raw_os_error()),
        };
        stage(directory, &role, &format!("명시 분리 결과 확인: 오류={blocked:?}, 일반 자식 생성"));
        let ordinary = fixture_command(directory, "sleeper").spawn().unwrap();
        publish(directory, &role, &SpawnReport { detached: detached.as_ref().map(std::process::Child::id), blocked, ordinary: ordinary.id() });
        stage(directory, &role, "자손 보고 게시 완료");
        std::thread::sleep(Duration::from_secs(30));
        if let Some(mut child) = detached { let _ = child.kill(); let _ = child.wait(); }
        let mut ordinary = ordinary;
        let _ = ordinary.kill();
        let _ = ordinary.wait();
    }
}
