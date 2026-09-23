//! Repository automation tasks, run with `cargo xtask <task>`.

mod check_deps;

use std::process::ExitCode;

const USAGE: &str = "\
Usage: cargo xtask <task>

Tasks:
  check-deps  Enforce crate licenses and dependency boundaries
  help        Print this message";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("check-deps") => match check_deps::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::FAILURE
            }
        },
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
