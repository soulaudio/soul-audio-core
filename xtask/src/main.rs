//! Repo automation. Run via the cargo alias: `cargo xtask <command>`.
//!
//! Pure Rust (no external task runner) so it behaves identically on Windows,
//! macOS, Linux, and CI. Git hooks are managed by lefthook (`lefthook.yml`);
//! the pre-push hook runs `cargo xtask verify`.

use std::process::{Command, exit};

mod resample_tables;

const USAGE: &str = "usage: cargo xtask <verify|ci|gen-resample-tables>";

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match cmd.as_str() {
        "verify" | "ci" => verify(),
        "gen-resample-tables" => resample_tables::run(),
        "check-resample-tables" => resample_tables::check(),
        "" => {
            eprintln!("{USAGE}");
            exit(2);
        }
        other => {
            eprintln!("unknown xtask command: {other:?}");
            eprintln!("{USAGE}");
            exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("xtask failed: {e}");
        exit(1);
    }
}

/// Run a cargo subcommand, returning an error string on non-zero exit.
fn run(args: &[&str]) -> Result<(), String> {
    println!("+ cargo {}", args.join(" "));
    let status = Command::new(env!("CARGO"))
        .args(args)
        .status()
        .map_err(|e| format!("spawning cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`cargo {}` failed ({status})", args.join(" ")))
    }
}

/// Full gate: format, lint, host tests, the no_std core build, and the firmware
/// build for the STM32 target. This is what CI and the pre-push hook run.
fn verify() -> Result<(), String> {
    run(&["fmt", "--all", "--", "--check"])?;
    run(&["clippy", "--all-targets", "--", "-D", "warnings"])?;
    // The checked-in resampler coefficient tables must match the generator.
    resample_tables::check()?;
    run(&["test"])?;
    // The STM32 discipline gates: the core and engine must build no_std for the
    // target, and the firmware must link for it.
    run(&[
        "build",
        "-p",
        "soul-audio-core",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
    run(&[
        "build",
        "-p",
        "soul-audio-engine",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
    run(&[
        "build",
        "-p",
        "soul-audio-firmware",
        "--target",
        "thumbv7em-none-eabihf",
    ])?;
    println!("verify: OK");
    Ok(())
}
