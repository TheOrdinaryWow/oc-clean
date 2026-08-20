pub mod types;

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "oc-clean",
    about = "Prune old OpenCode data and reclaim disk space"
)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "CLI switches are independent user choices"
)]
pub struct Cli {
    #[arg(long, env = "OCC_DB", value_name = "PATH")]
    pub db: Option<PathBuf>,

    #[arg(long)]
    pub apply: bool,

    #[arg(long)]
    pub force: bool,

    #[arg(long)]
    pub force_schema: bool,

    #[arg(long)]
    pub dangerously_skip_confirm: bool,

    #[arg(long)]
    pub skip_backup: bool,
}
