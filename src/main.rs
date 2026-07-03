use clap::{Parser, Subcommand};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "lamb", version, about = "LastAudioMemoryBuffer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon {
        #[arg(long)]
        config: PathBuf,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Recall {
        #[arg(long)]
        socket: PathBuf,
    },
    Clear {
        #[arg(long)]
        socket: PathBuf,
    },
    Status {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Stop {
        #[arg(long)]
        socket: PathBuf,
    },
    Dump {
        #[arg(long)]
        socket: PathBuf,
    },
    StartCapture {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        activate: bool,
    },
    StopCapture {
        #[arg(long)]
        socket: PathBuf,
    },
    Reload {
        #[arg(long)]
        socket: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Init {
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    Path {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Show {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Daemon { config } => lamb::daemon::run_from_config_path(&config),
        Command::Config { command } => run_config_command(command),
        Command::Recall { socket } => lamb::control::client_send_simple(&socket, "recall"),
        Command::Clear { socket } => lamb::control::client_send_simple(&socket, "clear"),
        Command::Status { socket, json } => lamb::control::client_status(&socket, json),
        Command::Stop { socket } => lamb::control::client_send_simple(&socket, "stop"),
        Command::Dump { socket } => lamb::control::client_dump(&socket),
        Command::StartCapture {
            socket,
            profile,
            activate,
        } => lamb::control::client_start_capture(&socket, profile, activate),
        Command::StopCapture { socket } => lamb::control::client_stop_capture(&socket),
        Command::Reload { socket } => lamb::control::client_reload(&socket),
    };

    if let Err(err) = result {
        eprintln!("lamb: {err}");
        std::process::exit(1);
    }
}

fn run_config_command(command: ConfigCommand) -> lamb::error::Result<()> {
    match command {
        ConfigCommand::Init { path, force } => {
            let path = resolve_config_path(path)?;
            lamb::app_config::write_default_config(&path, force)?;
            println!("{}", path.display());
            Ok(())
        }
        ConfigCommand::Path { path } => {
            let path = resolve_config_path(path)?;
            println!("{}", path.display());
            Ok(())
        }
        ConfigCommand::Show { path } => {
            let path = resolve_config_path(path)?;
            let text =
                fs::read_to_string(&path).map_err(|source| lamb::error::io_error(&path, source))?;
            print!("{text}");
            Ok(())
        }
    }
}

fn resolve_config_path(path: Option<PathBuf>) -> lamb::error::Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => lamb::app_config::default_config_path(),
    }
}
