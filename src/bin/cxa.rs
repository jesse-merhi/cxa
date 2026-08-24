use clap::Parser;
use cxa::cli::{Cli, run};
use cxa::config::Config;

fn main() {
    let result = Config::from_env().and_then(|config| run(Cli::parse(), config));
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
