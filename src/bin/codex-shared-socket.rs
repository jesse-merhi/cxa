use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    RecoverOwner { user: String },
    Prepare { user: String },
    Publish { user: String, pid: u32 },
}

fn main() {
    let result = cxa::socket_helper::require_root().and_then(|()| match Cli::parse().command {
        Command::RecoverOwner { user } => cxa::socket_helper::recover_owner(&user),
        Command::Prepare { user } => cxa::socket_helper::prepare(&user),
        Command::Publish { user, pid } => cxa::socket_helper::publish(&user, pid),
    });
    if let Err(error) = result {
        eprintln!("codex-shared-socket: {error}");
        std::process::exit(1);
    }
}
