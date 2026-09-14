use std::path::PathBuf;
use std::time::SystemTime;

use clap::{Parser, Subcommand};

use crate::error::Error;
use crate::orchestrator::{Environment, SystemEnvironment};
use crate::process::{ProcessSnapshot, ProcessState};
use crate::storage::Storage;
use crate::tui;

const TRACKED_COMMANDS: &[&str] = &[
    "cargo", "make", "gmake", "go", "npm", "npx", "pnpm", "pnpx", "yarn", "bun", "gradle",
    "gradlew", "mvn", "mvnw",
];

/// Classify command text without parsing or executing it.
pub(crate) fn should_track(command: &str) -> bool {
    fn word(input: &str, start: usize) -> Option<(&str, usize)> {
        let bytes = input.as_bytes();
        let mut end = start;
        while end < bytes.len() {
            let byte = bytes[end];
            if byte.is_ascii_whitespace() || b";|&<>()".contains(&byte) {
                break;
            }
            // An executable introduced through quoting or escaping is not a
            // literal token, even if the resulting shell word would match.
            if matches!(byte, b'\'' | b'"' | b'\\') {
                return None;
            }
            end += 1;
        }
        (end > start).then(|| (&input[start..end], end))
    }

    let bytes = command.as_bytes();
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let Some((first, mut end)) = word(command, start) else {
        return false;
    };
    let executable = if first == "sudo" {
        let separator = end;
        while end < bytes.len() && bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        if end == separator {
            return false;
        }
        let Some((second, _)) = word(command, end) else {
            return false;
        };
        second
    } else {
        first
    };
    if !TRACKED_COMMANDS.contains(&executable) {
        return false;
    }

    enum Quote {
        None,
        Single,
        AnsiC,
        Double,
        Backtick,
    }
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut parentheses = 0_usize;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        match quote {
            Quote::Single => {
                if byte == b'\'' {
                    quote = Quote::None;
                }
            }
            Quote::AnsiC => match byte {
                b'\'' => quote = Quote::None,
                b'\\' => escaped = true,
                _ => {}
            },
            Quote::Double => match byte {
                b'"' => quote = Quote::None,
                b'\\' => escaped = true,
                _ => {}
            },
            Quote::Backtick => match byte {
                b'`' => quote = Quote::None,
                b'\\' => escaped = true,
                _ => {}
            },
            Quote::None => match byte {
                b'\'' => {
                    let active_dollar = if index > 0 && bytes[index - 1] == b'$' {
                        let preceding_backslashes = bytes[..index - 1]
                            .iter()
                            .rev()
                            .take_while(|byte| **byte == b'\\')
                            .count();
                        preceding_backslashes % 2 == 0
                    } else {
                        false
                    };
                    quote = if active_dollar {
                        Quote::AnsiC
                    } else {
                        Quote::Single
                    };
                }
                b'"' => quote = Quote::Double,
                b'`' => quote = Quote::Backtick,
                b'\\' => escaped = true,
                b'(' => parentheses += 1,
                b')' => {
                    let Some(remaining) = parentheses.checked_sub(1) else {
                        return false;
                    };
                    parentheses = remaining;
                }
                b'&' => {
                    let previous = index.checked_sub(1).map(|i| bytes[i]);
                    let next = bytes.get(index + 1).copied();
                    let foreground_redirection =
                        matches!(previous, Some(b'<' | b'>')) || matches!(next, Some(b'>'));
                    if next == Some(b'&') {
                        index += 1;
                    } else if parentheses == 0 && !foreground_redirection {
                        return false;
                    }
                }
                _ => {}
            },
        }
        index += 1;
    }
    !escaped && parentheses == 0 && matches!(quote, Quote::None)
}

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
    if let Some(Command::Start { command }) = &cli.command {
        if cli.dry_run || !should_track(command) {
            println!("0");
            return Ok(());
        }
    }
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
        Command::Start { .. } => unreachable!("start previews return before opening storage"),
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
