#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Some(code) = agent_grid_lib::process_job::run_child() { std::process::exit(code); }
    if std::env::args().any(|arg| arg == "--self-test") {
        std::process::exit(agent_grid_lib::self_test::run());
    }
    if std::env::args().any(|arg| arg == "--codex-smoke") {
        std::process::exit(agent_grid_lib::self_test::run_codex_smoke());
    }
    agent_grid_lib::run();
}
