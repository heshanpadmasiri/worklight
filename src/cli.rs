use std::path::PathBuf;
use std::time::SystemTime;

use clap::{Parser, Subcommand};

use crate::error::Error;
use crate::orchestrator::{self, Environment, SystemEnvironment};
use crate::process::{ProcessSnapshot, ProcessState};
use crate::storage::Storage;
use crate::tui;

#[derive(Parser, Debug)]
#[command(name = "worklight", version, about = "Track manually started commands")]
pub(crate) struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    database: Option<PathBuf>,
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand, Debug)]
enum Command {
    Start {
        command: String,
    },
    Finish {
        #[arg(allow_hyphen_values = true)]
        id: i64,
        exit_code: i32,
    },
    Get {
        #[arg(allow_hyphen_values = true)]
        id: i64,
    },
    List {
        #[arg(long)]
        active: bool,
    },
    Acknowledge {
        #[arg(allow_hyphen_values = true)]
        id: i64,
    },
    Focus {
        #[arg(allow_hyphen_values = true)]
        id: i64,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        navigation_args: Vec<String>,
    },
    Panel,
}

pub(crate) fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    let path = cli
        .database
        .clone()
        .unwrap_or_else(|| default_database(&SystemEnvironment));
    if matches!(cli.command, None | Some(Command::Panel)) {
        return tui::run(&path, cli.dry_run);
    }
    let storage = if cli.dry_run {
        Storage::open_read_only(&path)?
    } else {
        Storage::open(&path)?
    };
    dispatch(cli.command.expect("handled panel"), &storage, cli.dry_run)
}
fn positive(id: i64) -> Result<i64, Error> {
    if id > 0 {
        Ok(id)
    } else {
        Err(Error::Invalid(format!("process id {id} must be positive")))
    }
}
fn dispatch(command: Command, storage: &Storage, dry_run: bool) -> Result<(), Error> {
    if dry_run {
        return preview(&command, storage);
    }
    match command {
        Command::Start { command } => println!("{}", storage.start_process(&command)?.id()),
        Command::Finish { id, exit_code } => {
            let id = positive(id)?;
            let status = u8::try_from(exit_code)
                .map_err(|_| Error::Invalid(format!("exit code {exit_code} is outside 0..=255")))?;
            let mut process = storage.get_process(id)?;
            process.finish(status)?;
            println!("{}", format_row(&process.snapshot()));
        }
        Command::Get { id } => println!(
            "{}",
            format_row(&storage.get_process(positive(id)?)?.snapshot())
        ),
        Command::List { active } => {
            for process in if active {
                storage.running_process()?
            } else {
                storage.all_process()?
            } {
                println!("{}", format_row(&process.snapshot()));
            }
        }
        Command::Acknowledge { id } => {
            let mut process = storage.get_process(positive(id)?)?;
            process.acknowledge()?;
            println!("{}", format_row(&process.snapshot()));
        }
        Command::Focus {
            id,
            navigation_args,
        } => {
            let mut process = storage.get_process(positive(id)?)?;
            let target = process.focus(&navigation_args)?;
            println!(
                "{}\t{}",
                escape_field(&target),
                format_row(&process.snapshot())
            );
        }
        Command::Panel => unreachable!(),
    }
    Ok(())
}
fn preview(command: &Command, storage: &Storage) -> Result<(), Error> {
    match command {
        Command::Start { command } => {
            if command.trim().is_empty() {
                return Err(Error::Invalid("command label is empty".into()));
            }
            orchestrator::detect(&SystemEnvironment).map_err(Error::Environment)?;
            println!("0");
        }
        Command::Finish { id, exit_code } => {
            let status = u8::try_from(*exit_code)
                .map_err(|_| Error::Invalid(format!("exit code {exit_code} is outside 0..=255")))?;
            let mut process = storage.get_process(positive(*id)?)?;
            match process.state()? {
                ProcessState::Running => {
                    let _ = status;
                }
                ProcessState::Finished { .. } => return Err(Error::AlreadyFinished(*id)),
                ProcessState::Acknowledged { .. } => return Err(Error::AlreadyAcknowledged(*id)),
            }
            println!("{}", format_row(&process.snapshot()));
        }
        Command::Get { id } => println!(
            "{}",
            format_row(&storage.get_process(positive(*id)?)?.snapshot())
        ),
        Command::List { active } => {
            for process in if *active {
                storage.running_process()?
            } else {
                storage.all_process()?
            } {
                println!("{}", format_row(&process.snapshot()));
            }
        }
        Command::Acknowledge { id } => {
            let mut process = storage.get_process(positive(*id)?)?;
            if matches!(process.state()?, ProcessState::Running) {
                return Err(Error::StillRunning(*id));
            }
            println!("{}", format_row(&process.snapshot()));
        }
        Command::Focus {
            id,
            navigation_args,
        } => {
            let mut process = storage.get_process(positive(*id)?)?;
            // Validate arguments without invoking tmux: shell ignores them, tmux
            // accepts only the documented client forms.
            if process.orchestrator().kind() == "tmux" {
                match navigation_args.as_slice() {
                    [] => {}
                    [a, b] if a == "--client" && !b.is_empty() => {}
                    [a] if a.starts_with("--client=") && a.len() > 9 => {}
                    _ => {
                        return Err(Error::Invalid(format!(
                            "unsupported tmux navigation arguments: {}",
                            navigation_args.join(" ")
                        )))
                    }
                }
            }
            process.state()?;
            let destination = format!(
                "{} ({})",
                process.orchestrator().describe(),
                process.orchestrator().cwd().display()
            );
            println!(
                "{}\t{}",
                escape_field(&destination),
                format_row(&process.snapshot())
            );
        }
        Command::Panel => unreachable!(),
    }
    Ok(())
}
pub(crate) fn format_row(process: &ProcessSnapshot) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        process.id,
        process.outcome(),
        process
            .exit_status()
            .map_or_else(|| "-".into(), |v| v.to_string()),
        process.elapsed(SystemTime::now()).as_millis(),
        if process.acknowledged() { "yes" } else { "no" },
        process.orchestrator.kind(),
        escape_field(&process.orchestrator.cwd().display().to_string()),
        escape_field(&process.label)
    )
}
fn escape_field(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}
pub(crate) fn default_database(env: &dyn Environment) -> PathBuf {
    if let Some(path) = env.var("WORKLIGHT_DB") {
        return PathBuf::from(path);
    }
    let base = env
        .var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env.var("HOME").unwrap_or_else(|| ".".into())).join(".local/share")
        });
    base.join("worklight/worklight.db")
}
