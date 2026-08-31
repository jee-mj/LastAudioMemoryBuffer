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
    Threshold {
        #[command(subcommand)]
        command: ThresholdCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ThresholdCommand {
    Calibrate {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        channel: String,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=30))]
        seconds: u32,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    Set {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        channel: String,
        #[arg(long, allow_hyphen_values = true)]
        dbfs: f64,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    Show {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    Reset {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        socket: Option<PathBuf>,
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
        Command::Recall { socket } => lamb::control::client_recall(&socket),
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
        Command::Threshold { command } => run_threshold_command(command),
    };

    if let Err(err) = result {
        eprintln!("lamb: {err}");
        std::process::exit(err.process_exit_code());
    }
}

fn run_threshold_command(command: ThresholdCommand) -> lamb::error::Result<()> {
    use lamb::control::ThresholdRequest;
    let (socket, request) = match command {
        ThresholdCommand::Calibrate {
            profile,
            channel,
            seconds,
            socket,
        } => (
            resolve_control_socket(socket)?,
            ThresholdRequest::Calibrate {
                profile,
                channel,
                seconds,
            },
        ),
        ThresholdCommand::Set {
            profile,
            channel,
            dbfs,
            socket,
        } => (
            resolve_control_socket(socket)?,
            ThresholdRequest::Set {
                profile,
                channel,
                dbfs,
            },
        ),
        ThresholdCommand::Show { profile, socket } => (
            resolve_control_socket(socket)?,
            ThresholdRequest::Show { profile },
        ),
        ThresholdCommand::Reset {
            profile,
            channel,
            socket,
        } => (
            resolve_control_socket(socket)?,
            ThresholdRequest::Reset { profile, channel },
        ),
    };
    lamb::control::client_threshold(&socket, request)
}

fn resolve_control_socket(socket: Option<PathBuf>) -> lamb::error::Result<PathBuf> {
    match socket {
        Some(socket) => Ok(socket),
        None => std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .map(|runtime| runtime.join("lamb/control.sock"))
            .ok_or_else(|| {
                lamb::error::LambError::Control(
                    "cannot resolve default control socket: XDG_RUNTIME_DIR is unavailable"
                        .to_string(),
                )
            }),
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
