use anstream::eprintln;
use clap::Parser;
use cxa::cli::{Cli, run};
use cxa::config::Config;
use cxa::terminal::ERROR;

fn main() {
    let cli = Cli::parse();
    let result = Config::from_env().and_then(|config| run(cli, config));
    if let Err(error) = result {
        eprintln!("{ERROR}error{ERROR:#}: {error}");
        std::process::exit(1);
    }
}
