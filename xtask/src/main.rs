//! Repository automation tasks, run with `cargo xtask <task>`.

use std::process::ExitCode;

const USAGE: &str = "\
Usage: cargo xtask <task>

Tasks:
  help    Print this message

No tasks are implemented yet.";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown task: {other}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
