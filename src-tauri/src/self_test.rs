//! Executable-level PTY checks. This runs before Tauri/WebView initialization and
//! never invokes the vendor adapter, model APIs, user configuration or real apps.
use crate::{flow::{MAX_PENDING_BYTES, MAX_PENDING_FRAMES}, process_job, session::{BackendEvent, Engine, EventSink, SessionInfo, StartRequest}};
use portable_pty::CommandBuilder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::{Path, PathBuf}, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, mpsc, Arc, Mutex}, thread, time::{Duration, Instant}};
use uuid::Uuid;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemorySample { elapsed_seconds: f64, working_set_bytes: Option<usize>, private_bytes: Option<usize>, handles: Option<u32>, owned_processes: Option<u32> }

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    schema: &'static str,
    tests: u64,
    pass: u64,
    fail: u64,
    duration_seconds: f64,
    iterations: u64,
    sessions_started: u64,
    output_frames: u64,
    output_bytes: u64,
    blocked_input_observed: Option<bool>,
    sample_memory: Vec<MemorySample>,
    errors: Vec<String>,
    scope: &'static str,
}

struct Harness {
    engine: Engine,
    output: Arc<Mutex<HashMap<String, String>>>,
    output_bytes: Arc<AtomicU64>,
    output_frames: Arc<AtomicU64>,
    ack: Arc<AtomicBool>,
    collector_stop: Arc<AtomicBool>,
    collector: Option<thread::JoinHandle<()>>,
    errors: Arc<Mutex<Vec<String>>>,
    capture: mpsc::SyncSender<Capture>,
    sessions_started: Arc<AtomicU64>,
}

enum Capture { Event(BackendEvent), Barrier(mpsc::Sender<()>) }

impl Harness {
    fn new() -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(64);
        let sink_sender = sender.clone();
        let sink: EventSink = Arc::new(move |event| sink_sender.send(Capture::Event(event)).map_err(|_| "fixture renderer stopped".into()));
        let engine = Engine::new(Path::new(":memory:"), sink)?;
        let output = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let output_bytes = Arc::new(AtomicU64::new(0));
        let output_frames = Arc::new(AtomicU64::new(0));
        let ack = Arc::new(AtomicBool::new(true));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let collector_stop = Arc::new(AtomicBool::new(false));
        let (writing, bytes, frames, acknowledging, failures, stopping, backend) = (output.clone(), output_bytes.clone(), output_frames.clone(), ack.clone(), errors.clone(), collector_stop.clone(), engine.clone());
        let collector = thread::spawn(move || {
            let mut query_tails = HashMap::<String, String>::new();
            while !stopping.load(Ordering::Acquire) {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(Capture::Event(BackendEvent::Output(event))) => {
                        if acknowledging.load(Ordering::Acquire) {
                            if let Err(error) = backend.acknowledge(&event.id, event.generation, event.seq) { failures.lock().unwrap().push(error); }
                        }
                        // portable-pty enables PSEUDOCONSOLE_INHERIT_CURSOR.
                        // A real xterm replies itself; this headless renderer must
                        // answer the same DSR or ClosePseudoConsole can deadlock.
                        let tail = query_tails.entry(event.id.clone()).or_default();
                        let scan = tail.clone() + &event.data;
                        *tail = scan.chars().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
                        for _ in 0..scan.matches("\x1b[6n").count() {
                            let _ = backend.write(&event.id, event.generation, "\x1b[1;1R");
                        }
                        let mut entries = writing.lock().unwrap();
                        let text = entries.entry(event.id).or_default();
                        // Keep enough to inspect all fixture markers, independent
                        // of emitted volume in backpressure/long-running checks.
                        if text.len() < 64 * 1024 { text.push_str(&event.data); }
                        bytes.fetch_add(event.data.len() as u64, Ordering::Relaxed);
                        frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Capture::Event(BackendEvent::Changed(info))) => { if matches!(info.state.as_str(), "exited" | "stopped" | "error") { query_tails.remove(&info.id); } }
                    Ok(Capture::Barrier(reply)) => { let _ = reply.send(()); }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Ok(Self { engine, output, output_bytes, output_frames, ack, collector_stop, collector: Some(collector), errors, capture: sender, sessions_started: Arc::new(AtomicU64::new(0)) })
    }

    fn start(&self, script: &str) -> Result<SessionInfo, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let mut command = shell(script)?;
        command.cwd(&cwd);
        // Regression input: a GUI/CI parent can advertise a dumb terminal.
        // Engine must declare the capabilities of its actual PTY renderer.
        command.env("TERM", "dumb");
        command.env("COLORTERM", "");
        let info = self.engine.start_fixture(StartRequest { cwd: cwd.to_string_lossy().into_owned(), title: "Headless PTY fixture".into(), thread_id: None, cols: 100, rows: 30 }, command)?;
        self.sessions_started.fetch_add(1, Ordering::Relaxed);
        Ok(info)
    }

    fn barrier(&self) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        self.capture.send(Capture::Barrier(sender)).map_err(|_| "fixture output collector stopped")?;
        receiver.recv_timeout(Duration::from_secs(5)).map_err(|_| "fixture output collector did not drain before timeout".into())
    }

    fn take_output(&self, info: &SessionInfo) -> Result<String, String> {
        // Native finish only means events were sent; this FIFO barrier proves
        // the renderer fixture consumed all earlier output before we inspect it.
        self.barrier()?;
        Ok(self.output.lock().unwrap().remove(&info.id).unwrap_or_default())
    }

    fn wait_output(&self, info: &SessionInfo, marker: &str, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.output.lock().unwrap().get(&info.id).is_some_and(|text| text.contains(marker)) { return Ok(()); }
            if Instant::now() >= deadline {
                let output = self.take_output(info)?;
                return Err(format!("fixture readiness marker {marker:?} was not observed; pure-fixture output tail={}", fixture_tail(&output)));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn isolation(&self, count: usize) -> Result<u64, String> {
        let mut sessions = Vec::new();
        let mut markers = Vec::new();
        for index in 0..count {
            let marker = format!("grid_{}_{}", Uuid::new_v4().simple(), index);
            let script = if cfg!(windows) {
                format!("1..8 | ForEach-Object {{ [Console]::WriteLine('{marker}'); Start-Sleep -Milliseconds 20 }}")
            } else { format!("for i in 1 2 3 4 5 6 7 8; do printf '{marker}\\n'; sleep 0.02; done") };
            sessions.push(self.start(&script)?);
            markers.push(marker);
        }
        for session in &sessions {
            self.engine.resize(&session.id, session.generation, 110, 32).or_else(|error| {
                // A very fast fixture may exit before resize. Resizing a terminal
                // that is already reaped must fail rather than touching another.
                if self.engine.wait_for_exit(&session.id, session.generation, Duration::from_millis(5)).is_ok() { Ok(()) } else { Err(error) }
            })?;
            self.engine.wait_for_exit(&session.id, session.generation, Duration::from_secs(30))?;
        }
        for (index, session) in sessions.iter().enumerate() {
            let output = self.take_output(session)?;
            if !output.contains(&markers[index]) { return Err(format!("session {index}/{count} did not receive its own output")); }
            if markers.iter().enumerate().any(|(other, marker)| other != index && output.contains(marker)) { return Err(format!("output crossed session boundary at {index}/{count}")); }
            if self.engine.write(&session.id, session.generation, "after-exit").is_ok() { return Err("write to a reaped process was accepted".into()); }
        }
        Ok(count as u64)
    }

    fn generation_and_lifecycle(&self) -> Result<u64, String> {
        let marker = format!("input_한글_🦀_{}", Uuid::new_v4().simple());
        // Windows PowerShell's default CP437 consumer demonstrably replaces
        // Korean/emoji with '?'. Give this fixture an explicit UTF-8 contract;
        // the actual vendor TUI owns its own terminal mode/encoding setup.
        let script = if cfg!(windows) { "[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::WriteLine('input-ready; encodings=' + [Console]::InputEncoding.CodePage + '/' + [Console]::OutputEncoding.CodePage + ';term=' + $env:TERM + ';color=' + $env:COLORTERM); $line = [Console]::ReadLine(); [Console]::WriteLine('received:' + $line)" } else { "printf 'input-ready;term=%s;color=%s\\n' \"$TERM\" \"$COLORTERM\"; read line; printf 'received:%s\\n' \"$line\"" };
        let session = self.start(script)?;
        self.wait_output(&session, "input-ready", Duration::from_secs(20))?;
        if !self.engine.list()?.iter().any(|info| info.id == session.id && info.active) { return Err("live PTY has no active ownership".into()); }
        let stale = session.generation + 1;
        if self.engine.write(&session.id, stale, "bad").is_ok() || self.engine.resize(&session.id, stale, 90, 20).is_ok() || self.engine.stop(&session.id, stale).is_ok() || self.engine.acknowledge(&session.id, stale, 1).is_ok() { return Err("stale generation request was accepted".into()); }
        self.engine.write(&session.id, session.generation, &(marker.clone() + "\r\n"))?;
        if self.engine.wait_for_exit(&session.id, session.generation, Duration::from_secs(30))?.active { return Err("reaped PTY retained active ownership".into()); }
        let output = self.take_output(&session)?;
        if !output.contains("term=xterm-256color;color=truecolor") { return Err(format!("PTY did not override the fixture's dumb terminal capabilities; pure-fixture output tail={}", fixture_tail(&output))); }
        if !output.contains(&("received:".to_string() + &marker)) { return Err(format!("PTY input did not reach the correct process; expected={marker:?}; pure-fixture output tail={}", fixture_tail(&output))); }
        let slow = self.start(if cfg!(windows) { "Start-Sleep -Seconds 60" } else { "sleep 60" })?;
        self.engine.stop(&slow.id, slow.generation)?;
        self.engine.stop(&slow.id, slow.generation)?; // idempotent cleanup
        let info = self.engine.wait_for_exit(&slow.id, slow.generation, Duration::from_secs(2))?;
        self.take_output(&slow)?;
        if info.state != "stopped" || info.active { return Err("stopped process has an invalid final state or active ownership".into()); }
        let failure = self.start("exit 7")?;
        let info = self.engine.wait_for_exit(&failure.id, failure.generation, Duration::from_secs(30))?;
        self.take_output(&failure)?;
        if info.state != "error" || info.active { return Err("nonzero child exit was not reported as an inactive error".into()); }
        Ok(3)
    }

    fn bounded_output_cancellation(&self) -> Result<u64, String> {
        self.ack.store(false, Ordering::Release);
        let before = self.output_bytes.load(Ordering::Acquire);
        let before_frames = self.output_frames.load(Ordering::Acquire);
        let script = if cfg!(windows) {
            "$s = 'x' * 4096; 1..1024 | ForEach-Object { [Console]::Write($s) }; Start-Sleep -Seconds 60"
        } else { "head -c 4194304 /dev/zero | tr '\\000' x; sleep 60" };
        let session = self.start(script)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.output_bytes.load(Ordering::Acquire) - before < (MAX_PENDING_BYTES - 32768) as u64 && self.output_frames.load(Ordering::Acquire) - before_frames < MAX_PENDING_FRAMES as u64 {
            if Instant::now() >= deadline { self.ack.store(true, Ordering::Release); self.engine.stop(&session.id, session.generation)?; return Err("output fixture never saturated its delivery window".into()); }
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(100));
        let emitted = self.output_bytes.load(Ordering::Acquire) - before;
        let frames = self.output_frames.load(Ordering::Acquire) - before_frames;
        if emitted > MAX_PENDING_BYTES as u64 || frames > MAX_PENDING_FRAMES as u64 { return Err(format!("unacknowledged output exceeded its limit: {emitted} bytes/{frames} frames")); }
        let start = Instant::now();
        self.engine.stop(&session.id, session.generation)?;
        self.ack.store(true, Ordering::Release);
        self.take_output(&session)?;
        if start.elapsed() > Duration::from_secs(35) { return Err("cancellation did not wake the output backpressure wait".into()); }
        Ok(1)
    }

    fn create_stop_cycles(&self, count: u64) -> Result<u64, String> {
        for _ in 0..count {
            let session = self.start(if cfg!(windows) { "Start-Sleep -Seconds 60" } else { "exec sleep 60" })?;
            self.engine.stop(&session.id, session.generation)?;
            let final_info = self.engine.wait_for_exit(&session.id, session.generation, Duration::from_secs(2))?;
            self.take_output(&session)?;
            if final_info.state != "stopped" { return Err("create/stop cycle did not reach stopped".into()); }
        }
        Ok(count)
    }

    fn process_tree_cleanup(&self) -> Result<u64, String> {
        #[cfg(windows)] {
            let baseline = process_job::app_owned_processes()?.ok_or("Windows job accounting is unavailable")?;
            let session = self.start("$p = Start-Process -FilePath ($env:SystemRoot + '\\System32\\WindowsPowerShell\\v1.0\\powershell.exe') -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60') -WindowStyle Hidden -PassThru; [Console]::WriteLine('owned-grandchild:' + $p.Id); Start-Sleep -Seconds 60")?;
            let deadline = Instant::now() + Duration::from_secs(20);
            while !self.output.lock().unwrap().get(&session.id).is_some_and(|text| text.contains("owned-grandchild:")) {
                if Instant::now() >= deadline { return Err("grandchild fixture never started".into()); }
                thread::sleep(Duration::from_millis(20));
            }
            let live = process_job::app_owned_processes()?.ok_or("Windows job accounting is unavailable")?;
            if live < baseline + 3 { return Err("the helper, shell, and grandchild were not all observed as owned processes".into()); }
            self.engine.stop(&session.id, session.generation)?;
            self.take_output(&session)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while process_job::app_owned_processes()? != Some(baseline) {
                if Instant::now() >= deadline { return Err("stopping a session left an owned grandchild running".into()); }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(1)
        }
        #[cfg(not(windows))] { Ok(0) }
    }

    fn blocked_input_stop(&self) -> Result<(u64, bool), String> {
        let baseline = process_job::app_owned_processes()?;
        let script = if cfg!(windows) { "[Console]::WriteLine('input-echo-ready'); $null = [Console]::ReadLine()" } else { "printf 'input-echo-ready\\n'; read line" };
        let session = self.start(script)?;
        self.wait_output(&session, "input-echo-ready", Duration::from_secs(20))?;
        let before_bytes = self.output_bytes.load(Ordering::Acquire);
        let before_frames = self.output_frames.load(Ordering::Acquire);
        // A cooked ReadLine echoes input without a terminating newline. Stop
        // consuming its output to propagate pressure back through the console.
        self.ack.store(false, Ordering::Release);
        let entered = Arc::new(AtomicU64::new(0));
        let completed = Arc::new(AtomicU64::new(0));
        let (result_sender, result_receiver) = mpsc::channel();
        let (attempts, writes, engine, id, generation) = (entered.clone(), completed.clone(), self.engine.clone(), session.id.clone(), session.generation);
        let worker = thread::spawn(move || {
            let input = "x".repeat(64 * 1024);
            // Bound total injected input even on hosts that eagerly buffer it.
            for _ in 0..256 {
                attempts.fetch_add(1, Ordering::Release);
                match engine.write(&id, generation, &input) {
                    Ok(()) => { writes.fetch_add(1, Ordering::Release); }
                    Err(error) => { let _ = result_sender.send(Err(error)); return; }
                }
            }
            let _ = result_sender.send(Ok(()));
        });
        // Observe an actual write call that stays in flight while no subsequent
        // call completes. Do not label an uncongested fixture a blocked-write test.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut blocked = false;
        loop {
            let before = completed.load(Ordering::Acquire);
            let saturated = self.output_bytes.load(Ordering::Acquire) - before_bytes >= (MAX_PENDING_BYTES - 32768) as u64 || self.output_frames.load(Ordering::Acquire) - before_frames >= MAX_PENDING_FRAMES as u64;
            if saturated && entered.load(Ordering::Acquire) > before {
                thread::sleep(Duration::from_millis(200));
                if completed.load(Ordering::Acquire) == before { blocked = true; break; }
            }
            if before >= 256 || Instant::now() >= deadline { break; }
            thread::sleep(Duration::from_millis(10));
        }
        let stopping = Instant::now();
        self.engine.stop(&session.id, session.generation)?;
        self.ack.store(true, Ordering::Release);
        let write_result = result_receiver.recv_timeout(Duration::from_secs(5)).map_err(|_| "stopping the session did not complete its bounded input worker")?;
        if blocked && write_result.is_ok() { return Err("a blocked input worker unexpectedly completed all 16 MiB after stop".into()); }
        worker.join().map_err(|_| "blocked input worker panicked")?;
        if stopping.elapsed() > Duration::from_secs(5) { return Err("blocked input cleanup exceeded five seconds".into()); }
        self.take_output(&session)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_job::app_owned_processes()? != baseline {
            if Instant::now() >= deadline { return Err("blocked input cleanup did not restore the owned process baseline".into()); }
            thread::sleep(Duration::from_millis(20));
        }
        Ok((1, blocked))
    }

    fn sustain(&self, start: Instant, duration: Duration, report: &mut Report) -> Result<(), String> {
        let script = if cfg!(windows) {
            "while (($line = [Console]::ReadLine()) -ne $null) { [Console]::WriteLine('reply:' + $line) }"
        } else { "while read line; do printf 'reply:%s\\n' \"$line\"; done" };
        let mut sessions = Vec::new();
        for _ in 0..8 { sessions.push(self.start(script)?); report.sessions_started += 1; }
        let sustained_start = Instant::now();
        let mut iteration = 0u64;
        let mut last_sample = Instant::now();
        // Even a short requested duration exercises one heartbeat across eight
        // concurrently live PTYs; long runs retain these exact processes.
        loop {
            iteration += 1;
            let markers: Vec<_> = sessions.iter().enumerate().map(|(index, _)| format!("sustain_{index}_{iteration}_{}", Uuid::new_v4().simple())).collect();
            for (index, session) in sessions.iter().enumerate() { self.engine.write(&session.id, session.generation, &(markers[index].clone() + "\r\n"))?; }
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let output = self.output.lock().unwrap();
                let complete = sessions.iter().enumerate().all(|(index, session)| output.get(&session.id).is_some_and(|text| text.contains(&("reply:".to_string() + &markers[index]))));
                drop(output);
                if complete { break; }
                if Instant::now() >= deadline { return Err("sustained PTY heartbeat timed out".into()); }
                thread::sleep(Duration::from_millis(10));
            }
            for (index, session) in sessions.iter().enumerate() {
                let output = self.take_output(session)?;
                if markers.iter().enumerate().any(|(other, marker)| other != index && output.contains(marker)) { return Err("sustained output crossed session boundary".into()); }
            }
            report.tests += 1;
            report.pass += 1;
            report.iterations += 1;
            if last_sample.elapsed() >= Duration::from_secs(30) {
                report.sample_memory.push(memory_sample(start));
                last_sample = Instant::now();
            }
            if sustained_start.elapsed() >= duration { break; }
            thread::sleep(Duration::from_millis(250));
        }
        for session in &sessions { self.engine.stop(&session.id, session.generation)?; self.take_output(session)?; }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        self.ack.store(true, Ordering::Release);
        let shutdown = self.engine.shutdown();
        if self.collector.is_some() { self.barrier()?; }
        self.collector_stop.store(true, Ordering::Release);
        if let Some(collector) = self.collector.take() { collector.join().map_err(|_| "fixture collector panicked")?; }
        shutdown?;
        let errors = self.errors.lock().unwrap();
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }
}
impl Drop for Harness { fn drop(&mut self) { let _ = self.finish(); } }

fn shell(script: &str) -> Result<CommandBuilder, String> {
    #[cfg(windows)] {
        let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is not defined")?;
        let mut command = CommandBuilder::new(PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script]);
        Ok(command)
    }
    #[cfg(not(windows))] {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        Ok(command)
    }
}

fn memory_sample(start: Instant) -> MemorySample {
    let mut sample = MemorySample { elapsed_seconds: start.elapsed().as_secs_f64(), working_set_bytes: None, private_bytes: None, handles: None, owned_processes: process_job::app_owned_processes().ok().flatten() };
    #[cfg(windows)] unsafe {
        use windows_sys::Win32::System::{ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX}, Threading::{GetCurrentProcess, GetProcessHandleCount}};
        let process = GetCurrentProcess();
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        counters.cb = std::mem::size_of_val(&counters) as u32;
        if K32GetProcessMemoryInfo(process, &mut counters as *mut _ as _, counters.cb) != 0 { sample.working_set_bytes = Some(counters.WorkingSetSize); sample.private_bytes = Some(counters.PrivateUsage); }
        let mut count = 0;
        if GetProcessHandleCount(process, &mut count) != 0 { sample.handles = Some(count); }
    }
    sample
}

fn fixture_tail(output: &str) -> String {
    // Only call for deterministic local fixture text, never actual vendor TUI.
    let tail: String = output.chars().rev().take(4096).collect::<Vec<_>>().into_iter().rev().collect();
    serde_json::to_string(&tail).unwrap_or_else(|_| "\"fixture tail serialization failed\"".into())
}

#[cfg(windows)]
fn unrelated_listener_receives_no_authentication() -> Result<(), String> {
    use crate::{codex, process_job::{LaunchGate, ProcessJob}};
    use std::{io::Read, net::TcpListener, os::windows::process::CommandExt, process::{Child, Stdio}};
    struct OwnedSleeper { child: Child, job: Option<ProcessJob>, gate: LaunchGate }
    impl Drop for OwnedSleeper {
        fn drop(&mut self) {
            if let Some(job) = self.job.take() { let _ = job.terminate(); }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    let baseline = process_job::app_owned_processes()?;
    // This listener belongs to the test executable's app Job, but deliberately
    // not to the dummy server's more specific owned Job.
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let endpoint = format!("ws://{}", listener.local_addr().map_err(|e| e.to_string())?);
    let executable = PathBuf::from(std::env::var_os("SystemRoot").ok_or("SystemRoot is not defined")?).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let (mut command, gate) = LaunchGate::wrap_std(executable.as_os_str())?;
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x08000000);
    let child = command.spawn().map_err(|e| format!("spawn isolated ownership fixture: {e}"))?;
    let mut sleeper = OwnedSleeper { child, job: None, gate };
    sleeper.job = Some(ProcessJob::assign_std(&sleeper.child)?);
    sleeper.gate.release()?;
    let receiving = thread::spawn(move || -> Result<usize, String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(format!("accept unrelated ownership fixture connection: {error}")),
            }
        };
        socket.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
        let mut bytes = [0u8; 1024];
        match socket.read(&mut bytes) {
            Ok(count) => Ok(count),
            Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => Ok(0),
            Err(error) => Err(format!("read unrelated ownership fixture connection: {error}")),
        }
    });
    // Exercise the actual production connect path with a synthetic capability,
    // rather than a duplicate ownership check that could miss a later HTTP send.
    let connection = codex::verify_connect_peer_fixture(&endpoint, &mut sleeper.child, sleeper.job.as_ref().expect("job assigned"));
    let received = receiving.join().map_err(|_| "ownership fixture listener panicked")??;
    let result = match connection {
        Err(error) if error.contains("connected TCP server does not belong to the owned server job") && received == 0 => Ok(()),
        Err(error) => Err(format!("ownership fixture did not reach the expected rejection; received {received} HTTP bytes: {error}")),
        Ok(()) => Err(format!("unrelated listener was accepted; received {received} HTTP bytes")),
    };
    drop(sleeper);
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_job::app_owned_processes()? != baseline {
        if Instant::now() >= deadline { return Err("ownership fixture did not restore its owned process baseline".into()); }
        thread::sleep(Duration::from_millis(20));
    }
    result
}

fn suite(duration: Duration) -> Report {
    let start = Instant::now();
    let mut report = Report { schema: "agent-grid-self-test-v1", tests: 0, pass: 0, fail: 0, duration_seconds: 0.0, iterations: 0, sessions_started: 0, output_frames: 0, output_bytes: 0, blocked_input_observed: None, sample_memory: vec![memory_sample(start)], errors: Vec::new(), scope: "Headless native PTY fixtures only. No GUI, vendor protocol, model API, or user-config validation. blockedInputObserved=false means the host did not demonstrate input backpressure; that case verifies bounded write/stop completion without claiming a blocked-write boundary." };
    let mut counters = None;
    let result = (|| -> Result<(), String> {
        process_job::install_app_job()?;
        let mut harness = Harness::new()?;
        counters = Some((harness.sessions_started.clone(), harness.output_frames.clone(), harness.output_bytes.clone()));
        #[cfg(windows)] {
            report.tests += 1;
            unrelated_listener_receives_no_authentication()?;
            report.pass += 1;
        }
        for count in [2, 8, 16] {
            report.tests += 1;
            report.sessions_started += harness.isolation(count)?;
            report.pass += 1;
        }
        report.tests += 1;
        report.sessions_started += harness.generation_and_lifecycle()?;
        report.pass += 1;
        report.tests += 1;
        report.sessions_started += harness.bounded_output_cancellation()?;
        report.pass += 1;
        report.tests += 1;
        let (started, blocked) = harness.blocked_input_stop()?;
        report.sessions_started += started;
        report.blocked_input_observed = Some(blocked);
        report.pass += 1;
        #[cfg(windows)] {
            report.tests += 1;
            report.sessions_started += harness.process_tree_cleanup()?;
            report.pass += 1;
        }
        report.sample_memory.push(memory_sample(start));
        if !duration.is_zero() {
            report.tests += 1;
            report.sessions_started += harness.create_stop_cycles(100)?;
            report.pass += 1;
            harness.sustain(start, duration, &mut report)?;
        }
        harness.finish()?;
        report.output_bytes = harness.output_bytes.load(Ordering::Acquire);
        report.output_frames = harness.output_frames.load(Ordering::Acquire);
        // Windows job accounting includes this executable itself. Allow the OS
        // a bounded interval to finish accounting for terminated descendants.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match process_job::app_owned_processes()? {
                Some(count) if count != 1 => {
                    if Instant::now() >= deadline { return Err(format!("owned job still contains {count} processes after session cleanup")); }
                    thread::sleep(Duration::from_millis(20));
                }
                _ => break,
            }
        }
        Ok(())
    })();
    if let Err(error) = result { report.fail += 1; report.errors.push(error); }
    if let Some((sessions, frames, bytes)) = counters {
        report.sessions_started = sessions.load(Ordering::Acquire);
        report.output_frames = frames.load(Ordering::Acquire);
        report.output_bytes = bytes.load(Ordering::Acquire);
    }
    report.sample_memory.push(memory_sample(start));
    report.duration_seconds = start.elapsed().as_secs_f64();
    report
}
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let options = (|| -> Result<(Duration, Option<PathBuf>), String> {
        let mut duration = Duration::ZERO;
        let mut destination = None;
        let mut index = 1;
        while index < args.len() {
            match args[index].as_str() {
                "--self-test" => {}
                "--duration-seconds" => { index += 1; let seconds: u64 = args.get(index).ok_or("missing duration value")?.parse().map_err(|_| "invalid duration value")?; if seconds > 86400 { return Err("duration must not exceed 86400 seconds".into()); } duration = Duration::from_secs(seconds); }
                "--report" => { index += 1; destination = Some(PathBuf::from(args.get(index).ok_or("missing report path")?)); }
                unknown => return Err(format!("unknown self-test option: {unknown}")),
            }
            index += 1;
        }
        Ok((duration, destination))
    })();
    let (duration, destination) = match options { Ok(options) => options, Err(error) => { eprintln!("{error}"); return 2; } };
    let report = suite(duration);
    let text = match serde_json::to_string_pretty(&report) { Ok(text) => text, Err(error) => { eprintln!("serialize self-test report: {error}"); return 2; } };
    if let Some(destination) = destination {
        if let Err(error) = std::fs::write(&destination, text.as_bytes()) { eprintln!("write self-test report {}: {error}", destination.display()); return 2; }
    }
    println!("{text}");
    if report.fail == 0 { 0 } else { 1 }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SmokeSession {
    cwd: String,
    session_id: Option<String>,
    generation: Option<u64>,
    thread_id: Option<String>,
    observed_state: String,
    captured_pty_bytes: usize,
    captured_pty_sha256: String,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SmokeReport {
    schema: &'static str,
    pass: bool,
    duration_seconds: f64,
    test_directory: String,
    startup_mode: &'static str,
    preexisting_cwd: Option<String>,
    sessions: Vec<SmokeSession>,
    memory: Vec<MemorySample>,
    errors: Vec<String>,
    scope: &'static str,
}

/// Real adapter smoke with terminal-protocol replies only, without user text or
/// a model turn. This may stop
/// at the official CLI's trust/auth/first-run UI; that is an explicit failed
/// binding check, never permission to answer the prompt on the user's behalf.
pub fn run_codex_smoke() -> i32 {
    let options = (|| -> Result<(usize, PathBuf, bool, Option<PathBuf>), String> {
        let args: Vec<_> = std::env::args().collect();
        let mut count = 1;
        let mut report = None;
        let mut diagnostic_terminal = false;
        let mut existing_cwd = None;
        let mut index = 1;
        while index < args.len() {
            match args[index].as_str() {
                "--codex-smoke" => {}
                "--diagnostic-terminal" => diagnostic_terminal = true,
                "--cwd" => { index += 1; existing_cwd = Some(PathBuf::from(args.get(index).ok_or("missing existing working directory")?)); }
                "--sessions" => { index += 1; count = args.get(index).ok_or("missing sessions count")?.parse().map_err(|_| "invalid sessions count")?; }
                "--report" => { index += 1; report = Some(PathBuf::from(args.get(index).ok_or("missing report path")?)); }
                other => return Err(format!("unknown Codex smoke option: {other}")),
            }
            index += 1;
        }
        if !(1..=16).contains(&count) { return Err("smoke sessions must be in 1..16".into()); }
        let report = report.ok_or("Codex smoke requires --report <absolute-path>")?;
        if !report.is_absolute() || !report.parent().is_some_and(Path::is_dir) { return Err("Codex smoke report must be absolute with an existing parent directory".into()); }
        if let Some(cwd) = &existing_cwd {
            if !cwd.is_absolute() || !cwd.is_dir() { return Err("Codex smoke --cwd must be an absolute existing directory".into()); }
            let source = cwd.canonicalize().map_err(|e| format!("verify existing smoke directory: {e}"))?;
            let artifacts = report.parent().expect("report parent was validated").canonicalize().map_err(|e| format!("verify smoke artifact directory: {e}"))?;
            if artifacts.starts_with(source) { return Err("Codex smoke report artifacts must be outside the provided working directory".into()); }
        }
        Ok((count, report, diagnostic_terminal, existing_cwd))
    })();
    let (count, destination, diagnostic_terminal, existing_cwd) = match options { Ok(options) => options, Err(error) => { eprintln!("{error}"); return 2; } };
    let start = Instant::now();
    let directory = destination.parent().expect("report parent was validated").join(format!("agent-grid-codex-smoke-{}", Uuid::new_v4().simple()));
    let mut report = SmokeReport { schema: "agent-grid-codex-smoke-v1", pass: false, duration_seconds: 0.0, test_directory: directory.to_string_lossy().into_owned(), startup_mode: if existing_cwd.is_some() { "preexisting-cwd-read-only-startup" } else { "isolated-fresh-cwd-startup" }, preexisting_cwd: existing_cwd.as_ref().map(|cwd| cwd.to_string_lossy().into_owned()), sessions: Vec::new(), memory: Vec::new(), errors: Vec::new(), scope: "Actual owned Codex app-server/remote TUI startup and observed thread/status binding only. Only terminal protocol negotiation replies; no user text, model turns, approval decisions, user config writes, or GUI validation. A provided cwd is used for read-only startup; the harness writes artifacts only to its separate report/test directory. The report saves PTY hashes only; --diagnostic-terminal explicitly saves a separate bounded redacted startup prefix locally." };
    let outcome = (|| -> Result<(), String> {
        process_job::install_app_job()?;
        let baseline = process_job::app_owned_processes()?;
        report.memory.push(memory_sample(start));
        std::fs::create_dir_all(&directory).map_err(|e| format!("create isolated smoke directory: {e}"))?;
        let mut harness = Harness::new()?;
        let mut starters = Vec::new();
        for index in 0..count {
            let cwd = if let Some(cwd) = &existing_cwd { cwd.clone() } else {
                let cwd = directory.join(format!("session-{index:02}"));
                std::fs::create_dir(&cwd).map_err(|e| format!("create smoke working directory: {e}"))?;
                cwd
            };
            let engine = harness.engine.clone();
            let title = format!("Codex smoke {index}");
            let path = cwd.to_string_lossy().into_owned();
            let request = StartRequest { cwd: path.clone(), title, thread_id: None, cols: 100, rows: 30 };
            starters.push((path, thread::spawn(move || engine.start(request))));
        }
        let mut started = Vec::new();
        for (cwd, starter) in starters {
            match starter.join() {
                Ok(Ok(session)) => started.push(session),
                result => {
                    let error = match result { Ok(Err(error)) => error, _ => "Codex startup worker panicked".into() };
                    report.sessions.push(SmokeSession { cwd, session_id: None, generation: None, thread_id: None, observed_state: "error".into(), captured_pty_bytes: 0, captured_pty_sha256: format!("{:x}", Sha256::digest([])), error: Some(error) });
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let sessions = harness.engine.list()?;
            let output = harness.output.lock().unwrap();
            let bound = started.iter().all(|original| sessions.iter().find(|info| info.id == original.id).is_some_and(|info| info.thread_id.as_ref().is_some_and(|id| Uuid::parse_str(id).is_ok()) && matches!(info.state.as_str(), "idle" | "running" | "waiting")) && output.get(&original.id).is_some_and(|text| !text.is_empty()));
            drop(output);
            if bound || Instant::now() >= deadline { break; }
            thread::sleep(Duration::from_millis(100));
        }
        report.memory.push(memory_sample(start));
        let snapshots = harness.engine.list()?;
        for original in &started {
            let info = snapshots.iter().find(|info| info.id == original.id).unwrap_or(original);
            let valid = info.thread_id.as_ref().is_some_and(|id| Uuid::parse_str(id).is_ok()) && matches!(info.state.as_str(), "idle" | "running" | "waiting");
            let output = harness.take_output(info)?;
            if diagnostic_terminal {
                // Explicit local-only diagnostic for these newly created TUIs.
                // Default reports remain hashes; this artifact is never emitted
                // to stdout and is not part of the packaging/upload manifest.
                let prefix: String = output.chars().take(8192).collect();
                let escaped = serde_json::to_vec_pretty(&serde_json::json!({"sessionId":info.id,"redactedStartupPrefix":redact_terminal(&prefix)})).map_err(|e| format!("encode local terminal diagnostic: {e}"))?;
                let path = directory.join(format!("startup-terminal-{}.json", info.id));
                std::fs::write(path, escaped).map_err(|e| format!("write local terminal diagnostic: {e}"))?;
            }
            let output_present = !output.is_empty();
            let error = if !valid { Some("exact UUID and observed live status were not both established within 45 seconds; trust/auth/first-run UI was not answered".into()) } else if !output_present { Some("remote TUI emitted no captured PTY output".into()) } else { None };
            report.sessions.push(SmokeSession { cwd: info.cwd.clone(), session_id: Some(info.id.clone()), generation: Some(info.generation), thread_id: info.thread_id.clone(), observed_state: info.state.clone(), captured_pty_bytes: output.len(), captured_pty_sha256: format!("{:x}", Sha256::digest(output.as_bytes())), error });
        }
        harness.finish()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_job::app_owned_processes()? != baseline {
            if Instant::now() >= deadline { return Err("owned process count did not return to its pre-smoke baseline".into()); }
            thread::sleep(Duration::from_millis(20));
        }
        if report.sessions.iter().any(|session| session.error.is_some()) { return Err("one or more real Codex sessions did not satisfy startup/binding smoke checks".into()); }
        Ok(())
    })();
    if let Err(error) = outcome { report.errors.push(error); }
    report.memory.push(memory_sample(start));
    report.duration_seconds = start.elapsed().as_secs_f64();
    report.pass = report.errors.is_empty() && report.sessions.len() == count && report.sessions.iter().all(|session| session.error.is_none());
    let text = match serde_json::to_string_pretty(&report) { Ok(text) => text, Err(error) => { eprintln!("serialize Codex smoke report: {error}"); return 2; } };
    if let Err(error) = std::fs::write(&destination, text.as_bytes()) { eprintln!("write Codex smoke report: {error}"); return 2; }
    println!("{text}");
    if report.pass { 0 } else { 1 }
}

fn redact_terminal(text: &str) -> String {
    let mut redacted = text.to_owned();
    for pattern in ["bearer ", "bearer\t", "sk-", "agent_grid_remote_token=", "token=", "token:", "token ", "api_key=", "api_key:", "apikey="] {
        let mut from = 0;
        while let Some(relative) = redacted[from..].to_ascii_lowercase().find(pattern) {
            let matched = from + relative;
            let mut start = matched + if pattern == "sk-" { 0 } else { pattern.len() };
            while redacted[start..].starts_with(|ch: char| matches!(ch, ' ' | '\t' | '"' | '\'')) { start += 1; }
            let end = redacted[start..].find(|ch: char| matches!(ch, ' ' | '\t' | '\r' | '\n' | '"' | '\'' | '`' | ';' | '<' | '>')).map(|length| start + length).unwrap_or(redacted.len());
            if start == end { from = (matched + pattern.len()).min(redacted.len()); }
            else { redacted.replace_range(start..end, "[REDACTED]"); from = start + "[REDACTED]".len(); }
        }
    }
    redacted
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_terminal_diagnostics_redact_credentials_and_keep_control_text() {
        let result = super::redact_terminal("\x1b[31mError\x1b[0m Bearer abc.def token=secret sk-example-token AGENT_GRID_REMOTE_TOKEN=capability\nnext");
        assert!(result.contains("\x1b[31mError\x1b[0m"));
        for secret in ["abc.def", "secret", "sk-example-token", "capability"] { assert!(!result.contains(secret)); }
        assert!(result.ends_with("\nnext"));
    }
}
