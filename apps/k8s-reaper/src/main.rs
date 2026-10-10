use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "k8s-reaper", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the reaper (default).
    Run,
    /// Print the ReaperConfig CRD as YAML.
    Crd,
}

fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run | Command::Crd => {}
    }
}
