//! Worklight: manual tracking of user-started commands.

mod agent;
mod cli;
mod error;
mod orchestrator;
mod process;
mod storage;
mod tui;

#[cfg(test)]
mod tests;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("worklight: {error}");
        std::process::exit(1);
    }
}
