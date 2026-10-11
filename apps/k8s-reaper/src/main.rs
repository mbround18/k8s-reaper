use std::net::SocketAddr;

use clap::{Args, Parser, Subcommand};
use reaper_observability::tracing::LogFormat;

mod run;

#[derive(Parser)]
#[command(name = "k8s-reaper", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the reaper (default).
    Run,
    /// Print the ReaperConfig CRD as YAML.
    Crd,
}

/// Process flags, per contracts/config.md. None of them change reaping behaviour.
#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Which ReaperConfig to read.
    #[arg(
        long,
        env = "REAPER_CONFIG_NAME",
        default_value = "default",
        global = true
    )]
    pub config_name: String,
    /// Own namespace, always excluded (set from the downward API).
    #[arg(long, env = "REAPER_NAMESPACE", global = true)]
    pub namespace: Option<String>,
    /// Address for `/healthz`, `/readyz` and `/metrics`.
    #[arg(
        long,
        env = "REAPER_HTTP_ADDR",
        default_value = "0.0.0.0:8080",
        global = true
    )]
    pub http_addr: SocketAddr,
    /// `json` or `text`.
    #[arg(long, env = "REAPER_LOG_FORMAT", default_value = "json", global = true)]
    pub log_format: LogFormat,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run::main(cli.run),
        Command::Crd => {
            print!("{}", reaper_kube::crd::crd_yaml());
            Ok(())
        }
    }
}
