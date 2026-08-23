//! Repo automation. Run via the cargo alias: `cargo xtask <command>`.
//!
//! Pure Rust (no external task runner) so it behaves identically on Windows,
//! macOS, Linux, and CI.

use std::fs;
use std::path::Path;
use std::process::{exit, Command};

const USAGE: &str = "usage: cargo xtask <verify|ci|install-hooks>";

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match cmd.as_str() {
        "verify" | "ci" => verify(),
        "install-hooks" => install_hooks(),
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
/// build for the STM32 target. This is what CI and the pre-commit hook run.
fn verify() -> Result<(), String> {
    run(&["fmt", "--all", "--", "--check"])?;
    run(&["clippy", "--all-targets", "--", "-D", "warnings"])?;
    run(&["test"])?;
    // The STM32 discipline gates: the core must build no_std for the target,
    // and the firmware must link for it.
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
        "soul-audio-firmware",
        "--target",
        "thumbv7em-none-eabihf",
    ])?;
    println!("verify: OK");
    Ok(())
}

/// Install a git pre-commit hook that runs `cargo xtask verify`.
fn install_hooks() -> Result<(), String> {
    let hook = Path::new(".git/hooks/pre-commit");
    fs::write(
        hook,
        "#!/bin/sh\n# installed by `cargo xtask install-hooks`\nexec cargo xtask verify\n",
    )
    .map_err(|e| format!("writing {}: {e}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(hook).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(hook, perms).map_err(|e| e.to_string())?;
    }
    println!("installed .git/hooks/pre-commit");
    Ok(())
}
