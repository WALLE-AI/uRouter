//! Cross-platform replacement for the repository's release-gate scripts.
//!
//! The gates used to be PowerShell, which made them unrunnable on the Linux and
//! macOS hosts the workspace otherwise builds on. Running them requires only a
//! Rust toolchain now, so the same gate executes locally and in CI.

mod chaos;
mod deploy;
mod pricing_fixture;
mod pure_crates;
mod secrets;

use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitCode};

use clap::{Parser, Subcommand};

type BoxError = Box<dyn std::error::Error>;

#[derive(Parser)]
#[command(name = "urouter-xtask", about = "uRouter repository release gates")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan tracked and untracked files for credential-shaped literals.
    CheckSecrets,
    /// Assert that the pure crates never gain an I/O dependency.
    CheckPureCrates,
    /// Assert that the golden pricing fixture has not shrunk.
    CheckPricingFixture,
    /// Assert that shipped deployment manifests pass the Gateway dry-run.
    CheckDeploy,
    /// Run the Redis restart and upstream failure gates against a Docker Redis.
    Chaos(chaos::ChaosArgs),
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = repository_root().and_then(|root| match args.command {
        Command::CheckSecrets => secrets::run(&root),
        Command::CheckPureCrates => pure_crates::run(),
        Command::CheckPricingFixture => pricing_fixture::run(&root),
        Command::CheckDeploy => deploy::run(&root),
        Command::Chaos(chaos) => chaos::run(&chaos, &root),
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Every gate resolves paths against the repository root so that it behaves the
/// same whether it is invoked from the root or from a crate directory.
pub fn repository_root() -> Result<PathBuf, BoxError> {
    let output = ProcessCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err("git rev-parse --show-toplevel failed".into());
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}
