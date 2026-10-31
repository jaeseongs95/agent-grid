//! Windows 실제 Job 경계 시험. 별도 앱 시험 프로세스와 자체 fixture만 사용한다.
//! 먼저 `cargo build --manifest-path src-tauri/Cargo.toml --bin opendiy-desktop-helper`가 필요하다.
use super::{install_app_job, LaunchGate, ProcessJob};
use std::{
    fs,
    os::windows::{io::{AsRawHandle, FromRawHandle, OwnedHandle}, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{OpenProcess, TerminateProcess, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE},
};

const FIXTURE: &str = "process_job::tests::job_fixture";
const TIMEOUT: Duration = Duration::from_secs(10);

/// 시험이 직접 생성한 프로세스만 열고, 실패 경로에서도 정리한다.
struct FixtureProcess { handle: OwnedHandle, cleanup: bool }
impl FixtureProcess {
    fn open(pid: u32) -> Self {
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
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
fn receive<T: serde::de::DeserializeOwned>(directory: &Path, name: &str) -> T {
    let started = Instant::now();
    loop {
        match fs::read(directory.join(name)) {
            Ok(bytes) => return serde_json::from_slice(&bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                assert!(started.elapsed() < TIMEOUT, "시험 응답 {name} 시간 초과");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("시험 응답 {name} 읽기: {error}"),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SpawnReport { detached: Option<u32>, blocked: Option<i32>, ordinary: u32 }

fn check_launcher(job: ProcessJob, helper: &FixtureProcess, directory: &Path, role: &str) -> Option<FixtureProcess> {
    let report: SpawnReport = receive(directory, role);
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
    drop(job);
    helper.await_exit();
    ordinary.await_exit();
    if let Some(process) = &detached { assert!(process.running(), "전용 실행기 Job 종료가 daemon을 종료했습니다."); }
    detached
}

fn app_fixture(directory: &Path) {
    // 전체 시험 실행기를 APP_JOB에 넣지 않도록 이 fixture 프로세스에서만 설치한다.
    install_app_job().unwrap();
    pty_fixture(directory);
    let executable = std::env::current_exe().unwrap();
    for role in ["query", "daemon"] {
        let (mut command, gate) = LaunchGate::wrap_std(executable.as_os_str()).unwrap();
        configure(&mut command, directory, role);
        let mut child = command.spawn().unwrap();
        let helper = FixtureProcess::open(child.id());
        let job = if role == "daemon" { ProcessJob::assign_daemon_launcher(&child) } else { ProcessJob::assign_std(&child) }.unwrap();
        gate.release().unwrap();
        let detached = check_launcher(job, &helper, directory, role);
        child.wait().unwrap();
        if let Some(detached) = detached {
            // launcher Job이 닫힌 뒤에도 앱 Job만 상속한 일반 자식은 살아 있다.
            let child = fixture_command(directory, "sleeper").spawn().unwrap();
            let ordinary = FixtureProcess::open(child.id());
            assert!(ordinary.running());
            let report: SpawnReport = receive(directory, "daemon");
            publish(directory, "app-ready", &(report.detached.unwrap(), child.id()));
            let _: bool = receive(directory, "finish-app");
            detached.hand_off();
            ordinary.hand_off();
        }
    }
}

fn pty_fixture(directory: &Path) {
    let pair = portable_pty::native_pty_system().openpty(portable_pty::PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
    command.args(fixture_args());
    command.env("OPENDIY_JOB_TEST_DIRECTORY", directory);
    command.env("OPENDIY_JOB_TEST_ROLE", "pty");
    let (command, gate) = LaunchGate::wrap_pty(command).unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    let helper = FixtureProcess::open(child.process_id().unwrap());
    let job = ProcessJob::assign_pty(child.as_ref()).unwrap();
    gate.release().unwrap();
    assert!(check_launcher(job, &helper, directory, "pty").is_none());
    child.wait().unwrap();
}

#[test]
fn only_daemon_launcher_can_outlive_launcher_and_app_jobs() {
    let directory = TestDirectory(std::env::temp_dir().join(format!("opendiy-job-test-{}", uuid::Uuid::new_v4())));
    fs::create_dir(&directory.0).unwrap();
    let mut app = fixture_command(&directory.0, "app").stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap();
    let app_process = FixtureProcess::open(app.id());
    let (daemon_pid, ordinary_pid): (u32, u32) = receive(&directory.0, "app-ready");
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
    if role == "app" {
        app_fixture(directory);
    } else if role == "sleeper" {
        // 시험 도중 부모가 강제 종료돼도 fixture는 무한히 남지 않는다.
        std::thread::sleep(Duration::from_secs(30));
    } else {
        assert!(matches!(role.as_str(), "query" | "pty" | "daemon"));
        let mut detached_command = fixture_command(directory, "sleeper");
        detached_command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        let detached = detached_command.spawn();
        let (detached, blocked) = match detached {
            Ok(child) => (Some(child), None),
            Err(error) => (None, error.raw_os_error()),
        };
        let ordinary = fixture_command(directory, "sleeper").spawn().unwrap();
        publish(directory, &role, &SpawnReport { detached: detached.as_ref().map(std::process::Child::id), blocked, ordinary: ordinary.id() });
        std::thread::sleep(Duration::from_secs(30));
        if let Some(mut child) = detached { let _ = child.kill(); let _ = child.wait(); }
        let mut ordinary = ordinary;
        let _ = ordinary.kill();
        let _ = ordinary.wait();
    }
}
