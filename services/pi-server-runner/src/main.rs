use clap::Parser;

/// Headless host runner for exercising the in-process pi coding agent runtime.
#[derive(Parser, Debug)]
#[command(name = "pi-server-runner", version, about, long_about = None)]
struct Cli {}

fn main() {
    let _ = Cli::parse();
}
