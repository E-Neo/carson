use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "carson", version, about = "carson wasm agent host")]
pub struct Cli {
    /// Override the carson home directory
    #[arg(long, env = "CARSON_HOME", value_name = "DIR")]
    pub home: Option<PathBuf>,
}
