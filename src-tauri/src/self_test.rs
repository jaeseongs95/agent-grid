//! Executable-level PTY checks. This runs before Tauri/WebView initialization and
//! never invokes the vendor adapter, model APIs, user configuration or real apps.
use crate::{flow::MAX_PENDING_BYTES, process_job, session::{BackendEvent, Engine, EventSink, SessionInfo, StartRequest}};
use portable_pty::CommandBuilder;
use serde::Serialize;
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
}

impl Harness {
    fn new() -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(64);
        let sink: EventSink = Arc::new(move |event| sender.send(event).map_err(|_| "fixture renderer stopped".into()));
        let engine = Engine::new(Path::new(":memory:"), sink)?;
        let output = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let output_bytes = Arc::new(AtomicU64::new(0));
        let output_frames = Arc::new(AtomicU64::new(0));
        let ack = Arc::new(AtomicBool::new(true));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let collector_stop = Arc::new(AtomicBool::new(false));
        let (writing, bytes, frames, acknowledging, failures, stopping, backend) = (output.clone(), output_bytes.clone(), output_frames.clone(), ack.clone(), errors.clone(), collector_stop.clone(), engine.clone());
        let collector = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(BackendEvent::Output(event)) => {
                        if acknowledging.load(Ordering::Acquire) {
                            if let Err(error) = backend.acknowledge(&event.id, event.generation, event.seq) { failures.lock().unwrap().push(error); }
                        }
                        let mut entries = writing.lock().unwrap();
                        let text = entries.entry(event.id).or_default();
                        // Keep enough to inspect all fixture markers, independent
                        // of emitted volume in backpressure/long-running checks.
                        if text.len() < 64 * 1024 { text.push_str(&event.data); }
                        bytes.fetch_add(event.data.len() as u64, Ordering::Relaxed);
                        frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(BackendEvent::Changed(_)) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Ok(Self { engine, output, output_bytes, output_frames, ack, collector_stop, collector: Some(collector), errors })
    }

    fn start(&self, script: &str) -> Result<SessionInfo, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let mut command = shell(script)?;
        command.cwd(&cwd);
        self.engine.start_fixture(StartRequest { cwd: cwd.to_string_lossy().into_owned(), title: "Headless PTY fixture".into(), thread_id: None, cols: 100, rows: 30 }, command)
    }

    fn take_output(&self, info: &SessionInfo) -> String { self.output.lock().unwrap().remove(&info.id).unwrap_or_default() }

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
            let output = self.take_output(session);
            if !output.contains(&markers[index]) { return Err(format!("session {index}/{count} did not receive its own output")); }
            if markers.iter().enumerate().any(|(other, marker)| other != index && output.contains(marker)) { return Err(format!("output crossed session boundary at {index}/{count}")); }
            if self.engine.write(&session.id, session.generation, "after-exit").is_ok() { return Err("write to a reaped process was accepted".into()); }
        }
        Ok(count as u64)
    }

    fn generation_and_lifecycle(&self) -> Result<u64, String> {
        let marker = format!("input_{}", Uuid::new_v4().simple());
        let script = if cfg!(windows) { "$line = [Console]::ReadLine(); [Console]::WriteLine('received:' + $line)" } else { "read line; printf 'received:%s\\n' \"$line\"" };
        let session = self.start(script)?;
        let stale = session.generation + 1;
        if self.engine.write(&session.id, stale, "bad").is_ok() || self.engine.resize(&session.id, stale, 90, 20).is_ok() || self.engine.stop(&session.id, stale).is_ok() || self.engine.acknowledge(&session.id, stale, 1).is_ok() { return Err("stale generation request was accepted".into()); }
        self.engine.write(&session.id, session.generation, &(marker.clone() + "\r\n"))?;
        self.engine.wait_for_exit(&session.id, session.generation, Duration::from_secs(30))?;
        if !self.take_output(&session).contains(&("received:".to_string() + &marker)) { return Err("PTY input did not reach the correct process".into()); }
        let slow = self.start(if cfg!(windows) { "Start-Sleep -Seconds 60" } else { "sleep 60" })?;
        self.engine.stop(&slow.id, slow.generation)?;
        self.engine.stop(&slow.id, slow.generation)?; // idempotent cleanup
        let info = self.engine.wait_for_exit(&slow.id, slow.generation, Duration::from_secs(2))?;
        self.take_output(&slow);
        if info.state != "stopped" { return Err("stopped process has an invalid final state".into()); }
        Ok(2)
    }

    fn bounded_output_cancellation(&self) -> Result<u64, String> {
        self.ack.store(false, Ordering::Release);
        let before = self.output_bytes.load(Ordering::Acquire);
        let script = if cfg!(windows) {
            "$s = 'x' * 4096; 1..1024 | ForEach-Object { [Console]::Write($s) }; Start-Sleep -Seconds 60"
        } else { "head -c 4194304 /dev/zero | tr '\\000' x; sleep 60" };
        let session = self.start(script)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.output_bytes.load(Ordering::Acquire) - before < (MAX_PENDING_BYTES - 32768) as u64 {
            if Instant::now() >= deadline { self.ack.store(true, Ordering::Release); self.engine.stop(&session.id, session.generation)?; return Err("output fixture never saturated its delivery window".into()); }
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(100));
        let emitted = self.output_bytes.load(Ordering::Acquire) - before;
        if emitted > MAX_PENDING_BYTES as u64 { return Err(format!("unacknowledged output exceeded its limit: {emitted}")); }
        let start = Instant::now();
        self.engine.stop(&session.id, session.generation)?;
        self.ack.store(true, Ordering::Release);
        self.take_output(&session);
        if start.elapsed() > Duration::from_secs(35) { return Err("cancellation did not wake the output backpressure wait".into()); }
        Ok(1)
    }

    fn create_stop_cycles(&self, count: u64) -> Result<u64, String> {
        for _ in 0..count {
            let session = self.start(if cfg!(windows) { "Start-Sleep -Seconds 60" } else { "exec sleep 60" })?;
            self.engine.stop(&session.id, session.generation)?;
            let final_info = self.engine.wait_for_exit(&session.id, session.generation, Duration::from_secs(2))?;
            self.take_output(&session);
            if final_info.state != "stopped" { return Err("create/stop cycle did not reach stopped".into()); }
        }
        Ok(count)
    }

    fn sustain(&self, start: Instant, duration: Duration, report: &mut Report) -> Result<(), String> {
        let script = if cfg!(windows) {
            "while (($line = [Console]::ReadLine()) -ne $null) { [Console]::WriteLine('reply:' + $line) }"
        } else { "while read line; do printf 'reply:%s\\n' \"$line\"; done" };
        let mut sessions = Vec::new();
        for _ in 0..8 { sessions.push(self.start(script)?); report.sessions_started += 1; }
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
                let output = self.take_output(session);
                if markers.iter().enumerate().any(|(other, marker)| other != index && output.contains(marker)) { return Err("sustained output crossed session boundary".into()); }
            }
            report.tests += 1;
            report.pass += 1;
            report.iterations += 1;
            if last_sample.elapsed() >= Duration::from_secs(30) {
                report.sample_memory.push(memory_sample(start));
                last_sample = Instant::now();
            }
            if start.elapsed() >= duration { break; }
            thread::sleep(Duration::from_millis(250));
        }
        for session in &sessions { self.engine.stop(&session.id, session.generation)?; self.take_output(session); }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        self.ack.store(true, Ordering::Release);
        let shutdown = self.engine.shutdown();
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

fn suite(duration: Duration) -> Report {
    let start = Instant::now();
    let mut report = Report { schema: "agent-grid-self-test-v1", tests: 0, pass: 0, fail: 0, duration_seconds: 0.0, iterations: 0, sessions_started: 0, output_frames: 0, output_bytes: 0, sample_memory: vec![memory_sample(start)], errors: Vec::new(), scope: "Headless native PTY fixtures only. No GUI, vendor protocol, model API, or user-config validation." };
    let result = (|| -> Result<(), String> {
        process_job::install_app_job()?;
        let mut harness = Harness::new()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_ptys_isolate_output_reject_stale_generations_and_cancel_backpressure() {
        let report = suite(Duration::ZERO);
        assert_eq!(report.fail, 0, "{}", serde_json::to_string_pretty(&report).unwrap());
        assert_eq!(report.pass, 5);
        assert_eq!(report.sessions_started, 29);
    }
}
