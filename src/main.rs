use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "amm", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon
    Daemon,
    /// Run the MCP stdio shim
    Shim,
    /// Handle a Claude Code hook event
    Hook { event: String },
}

fn main() {
    let cli = Cli::parse();
    let name = match cli.command {
        Command::Daemon => "daemon".to_string(),
        Command::Shim => "shim".to_string(),
        Command::Hook { event } => format!("hook {event}"),
    };
    eprintln!("amm {name}: not implemented");
    std::process::exit(2);
}
