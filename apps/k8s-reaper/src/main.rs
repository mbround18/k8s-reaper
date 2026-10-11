//! The `k8s-reaper` binary: finds workloads stuck crash-looping and reports
//! or reaps them.
//!
//! A thin execution layer over the hexagon. It holds no decision logic: it
//! parses the process flags (`RunArgs`, per `contracts/config.md`), then
//! either prints the ReaperConfig CRD (`k8s-reaper crd`, from
//! `reaper_kube::crd::crd_yaml`) or runs the reaper (`k8s-reaper run`, the
//! default). `run::main` wires the adapters (`reaper-kube`, `reaper-flux`,
//! `reaper-observability`, `reaper-notify`) to `reaper-core` and loops:
//! snapshot, read config, `decide`, `execute`, notify, record metrics and
//! status, then wait for the next tick or a watched change.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use reaper_observability::tracing::LogFormat;

// Test-only hang hook; never in a release build.
#[cfg(debug_assertions)]
mod dev_hang;
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
    /// Directory of notification channel files (a mounted Secret); missing
    /// or empty means notifications are off.
    #[arg(
        long,
        env = "REAPER_NOTIFICATIONS_DIR",
        default_value = "/etc/k8s-reaper/notifications",
        global = true
    )]
    pub notifications_dir: PathBuf,
}

/// Errors propagate: returning one from `main` exits non-zero, so a loop that
/// ends or panics restarts the container (FR-001).
fn main() -> anyhow::Result<()> {
    dispatch(Cli::parse())
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run::main(cli.run),
        Command::Crd => {
            print!("{}", reaper_kube::crd::crd_yaml());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-001: an error from `run` comes out of `main`, which Rust turns into
    /// a non-zero exit, so the kubelet restarts the container. A missing
    /// namespace is the one error `run` can reach without a cluster.
    #[test]
    fn an_error_from_run_comes_out_of_main() {
        let cli = Cli {
            command: Some(Command::Run),
            run: RunArgs {
                config_name: "default".into(),
                namespace: None,
                http_addr: "127.0.0.1:0".parse().unwrap(),
                log_format: LogFormat::Text,
                notifications_dir: PathBuf::from("/nonexistent"),
            },
        };
        let err = dispatch(cli).expect_err("run fails without a namespace");
        assert!(err.to_string().contains("--namespace"), "{err:#}");
    }
}
