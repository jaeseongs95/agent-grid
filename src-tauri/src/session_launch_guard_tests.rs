use super::*;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

#[test]
fn failed_launch_guard_preserves_unknown_shared_stop_and_reaps_owned_pty() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    let sink = Arc::new(move |event| {
        if let BackendEvent::Changed(info) = event { sink_events.lock().unwrap().push(info.state); }
        Ok(())
    });
    let engine = Engine::new(Path::new(":memory:"), sink).unwrap();
    let request = StartRequest {cwd:std::env::current_dir().unwrap().to_string_lossy().into_owned(),title:"시작 정리 시험".into(),project_id:None,project_assigned:false,thread_id:None,cols:80,rows:24};
    let session = engine.allocate(&request).unwrap();
    // process 설치 뒤 launch가 실패한 경계만 직접 구성한다. 정규 reaper가 아직
    // 없는 단계이므로 LaunchGuard가 정리 결과와 최종 상태를 모두 인계해야 한다.
    let pair = native_pty_system().openpty(size(80, 24)).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let drainer = thread::spawn(move || { let _ = std::io::copy(&mut reader, &mut std::io::sink()); });
    let writer = pair.master.take_writer().unwrap();
    let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = CommandBuilder::new(shell);
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"]);
    let (command, gate) = LaunchGate::wrap_pty(command).unwrap();
    let mut child = OwnedChild {child:pair.slave.spawn_command(command).unwrap(),reaped:false};
    drop(pair.slave);
    let job = ProcessJob::assign_pty(child.child.as_ref()).unwrap();
    let handle = unsafe { OpenProcess(0x00100000, 0, child.child.process_id().unwrap()) };
    assert!(!handle.is_null());
    let original = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
    gate.release().unwrap();
    let process = Arc::new(ProcessControl {master:Mutex::new(Some(pair.master)),writer:Mutex::new(Some(writer)),killer:Mutex::new(child.child.clone_killer()),job:Mutex::new(Some(job)),adapter:Mutex::new(None),
        #[cfg(windows)] cron:Mutex::new(None),
        termination:Mutex::new(()),shared_cleanup_failed:AtomicBool::new(true),shared_stop_attempted:AtomicBool::new(true),_gate:gate});
    // 공유 RPC가 불명 결과를 기록한 인계 플래그만 주입한다. 이후 adapter가 이미
    // 회수되었더라도 추가 owned 정리가 그 불명을 성공으로 바꾸면 안 된다.
    *session.process.lock().unwrap() = Some(process.clone());
    session.request_stop().unwrap();
    drop(LaunchGuard {session:session.clone(),armed:true});
    assert_eq!(unsafe { WaitForSingleObject(original.as_raw_handle().cast(), 1000) }, 0, "원래 소유 PTY는 실패 정리 반환 전에 종료되어야 한다");
    child.wait().unwrap();
    drainer.join().unwrap();
    let info = session.info().unwrap();
    assert_eq!(info.state, "unknown", "공유 중단 불명을 inactive/stopped로 바꾸면 안 된다");
    assert!(!info.active);
    assert!(session.cleanup_failed.load(Ordering::Acquire));
    assert!(session.process.lock().unwrap().is_none());
    assert!(*session.done.0.lock().unwrap());
    assert!(!events.lock().unwrap().iter().any(|state| state == "stopped"));
    assert!(engine.stop(&info.id, info.generation).unwrap_err().contains("재전송하지 않았습니다"));
}
