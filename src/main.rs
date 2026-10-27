use amm::config::{DEFAULT_PORT, Env, amm_home};
use amm::daemon::{DaemonOpts, run_daemon};
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
    /// Handle a Claude Code hook event (stdin: the hook payload)
    Hook { event: Option<String> },
    /// Print the running daemon's dashboard URL
    Dashboard,
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // A hook must never fail on bad arguments: exit code 2 from PreToolUse would block the tool call.
        Err(_) if std::env::args().nth(1).as_deref() == Some("hook") => std::process::exit(0),
        Err(e) => e.exit(),
    };
    match cli.command {
        Command::Daemon => std::process::exit(daemon()),
        Command::Shim => std::process::exit(shim()),
        Command::Hook { event } => std::process::exit(amm::hook_client::run_hook(event.as_deref())),
        Command::Dashboard => std::process::exit(dashboard()),
    }
}

fn dashboard() -> i32 {
    let env: Env = std::env::vars().collect();
    match amm::daemon::read_lock(&amm_home(&env)).filter(amm::daemon::is_healthy) {
        Some(l) => {
            println!("http://127.0.0.1:{}/?token={}", l.port, l.token);
            0
        }
        None => {
            eprintln!("amm: no daemon running (it starts with a Claude Code session that uses amm)");
            1
        }
    }
}

fn shim() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("amm: {e}");
            return 1;
        }
    };
    match rt.block_on(amm::shim::run_shim()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("amm shim: {e}");
            1
        }
    }
}

fn daemon() -> i32 {
    let env: Env = std::env::vars().collect();
    let port = match env.get("AMM_PORT").map(|p| p.trim()).filter(|p| !p.is_empty()) {
        None => DEFAULT_PORT,
        Some(p) => match p.parse::<u16>() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("amm: bad AMM_PORT {p:?}");
                return 1;
            }
        },
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("amm: {e}");
            return 1;
        }
    };
    rt.block_on(async {
        use tokio::signal::unix::{SignalKind, signal};
        // Installed before the daemon starts, so an early signal is not lost to the default handler.
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
            eprintln!("amm: cannot install signal handlers");
            return 1;
        };
        let d = match run_daemon(DaemonOpts { home: amm_home(&env), port, ..Default::default() }).await {
            Ok(Some(d)) => d,
            Ok(None) => return 0,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        println!("amm daemon pid {} on 127.0.0.1:{}", std::process::id(), d.port());
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = d.closed() => return 0,
        }
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
            std::process::exit(1);
        });
        d.close().await;
        0
    })
}
