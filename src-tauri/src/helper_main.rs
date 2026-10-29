//! Deliberately retain the console subsystem in every build profile. A GUI
//! subsystem executable cannot serve as a ConPTY client reliably on Windows.
fn main() {
    match agent_grid_lib::process_job::run_child() {
        Some(code) => std::process::exit(code),
        None => {
            eprintln!("OpenDIY Desktop helper requires an owned launch gate");
            std::process::exit(2);
        }
    }
}
