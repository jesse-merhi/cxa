use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
struct Cli {
    upstream_socket: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = cxa::proxy::run(&cli.upstream_socket) {
        eprintln!("codex-quota-proxy: {error}");
        std::process::exit(1);
    }
}
