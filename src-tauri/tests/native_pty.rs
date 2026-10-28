#[test]
fn executable_uses_real_ptys_with_generation_fencing_and_bounded_cleanup() {
    let report = std::env::temp_dir().join(format!("agent-grid-native-{}.json", uuid::Uuid::new_v4()));
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_agent-grid"))
        .args(["--self-test", "--report"]).arg(&report).status().expect("run headless executable PTY checks");
    let text = std::fs::read_to_string(&report).expect("headless JSON report must exist");
    let result: serde_json::Value = serde_json::from_str(&text).expect("parse headless JSON report");
    assert!(status.success(), "{text}");
    assert_eq!(result["fail"], 0, "{text}");
    assert_eq!(result["pass"], if cfg!(windows) { 7 } else { 6 }, "{text}");
    assert_eq!(result["sessionsStarted"], if cfg!(windows) { 32 } else { 31 }, "{text}");
    std::fs::remove_file(report).expect("remove only the owned test report");
}
