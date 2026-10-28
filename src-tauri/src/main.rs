#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|arg| arg == "--self-test") {
        std::process::exit(agent_grid_lib::self_test::run());
    }
    agent_grid_lib::run();
}
