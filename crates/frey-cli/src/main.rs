//! The `frey` command line.
//!
//! Deliberately small, and deliberately machine-readable. The command that matters most is
//! `doctor`: a coding agent landing in an unfamiliar Frey project can run `frey doctor --json` and
//! orient in one step instead of reading the source. That is the whole reason the JSON shape is
//! treated as an API rather than as pretty output.
//!
//! No argument-parsing dependency. The surface is six commands and two flags, and a hand-written
//! parser is smaller than the crate that would parse it — as well as being one fewer thing in the
//! supply chain of a tool whose entire pitch includes supply-chain hygiene.

use std::process::ExitCode;

mod commands;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let positional: Vec<&str> =
        args.iter().filter(|a| !a.starts_with("--")).map(String::as_str).collect();

    match positional.first().copied() {
        None | Some("help" | "-h") => {
            print!("{}", commands::help());
            ExitCode::SUCCESS
        }
        Some("version") => {
            println!("frey {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("doctor") => commands::doctor(json),
        Some("profiles") => commands::profiles(json),
        Some("tools") => commands::tools(json),
        Some(unknown) => {
            eprintln!("unknown command `{unknown}`\n");
            eprint!("{}", commands::help());
            ExitCode::FAILURE
        }
    }
}
