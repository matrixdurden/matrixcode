mod app;
mod command;
mod session;
mod tui;

use std::env;
use std::process::ExitCode;

use app::App;

const NAME: &str = "MatrixCode";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("matrixcode {VERSION}");
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(arg) => {
            eprintln!("matrixcode: unknown argument: {arg}");
            eprintln!("Try 'matrixcode --help'.");
            ExitCode::from(2)
        }
        None => {
            let mut app = App::default();
            match tui::run(&mut app) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("matrixcode: {error}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn print_help() {
    println!("{NAME} {VERSION}");
    println!("Ultra-light terminal coding agent for Codex and Claude.");
    println!();
    println!("Usage: matrixcode [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -h, --help       Print help");
    println!("  -V, --version    Print version");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}
