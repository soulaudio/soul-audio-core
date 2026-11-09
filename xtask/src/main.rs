//! Repo automation. Run via the cargo alias: `cargo xtask <command>`.
//!
//! Pure Rust (no external task runner) so it behaves identically on Windows,
//! macOS, Linux, and CI. Git hooks are managed by lefthook (`lefthook.yml`);
//! the pre-push hook runs `cargo xtask verify`.

use std::path::Path;
use std::process::{Command, exit};

mod commit_msg;
mod filterbank_tables;
mod gate;
mod opus_tables;
mod ps_tables;
mod resample_tables;
mod sbr_tables;
mod silk_tables;
mod vorbis_tables;

const USAGE: &str = "usage: cargo xtask <verify|ci|precommit|qa-audio|check-commit-msg <file>|gen-resample-tables|gen-filterbank-tables|gen-sbr-tables|gen-ps-tables|gen-vorbis-tables|gen-opus-tables|gen-silk-tables>";

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match cmd.as_str() {
        // `verify` = the change-scoped gate (pre-push hook, dev PRs); `ci` = the
        // full authoritative gate (CI on dev->main and main).
        "verify" => gate::verify_scoped(&std::env::args().skip(2).collect::<Vec<_>>()),
        "ci" => verify(),
        "precommit" => gate::precommit(),
        "qa-audio" => qa_audio(),
        "check-commit-msg" => commit_msg::check(std::env::args().nth(2)),
        "gen-resample-tables" => resample_tables::run(),
        "check-resample-tables" => resample_tables::check(),
        "gen-filterbank-tables" => filterbank_tables::run(),
        "check-filterbank-tables" => filterbank_tables::check(),
        "gen-sbr-tables" => sbr_tables::run(),
        "check-sbr-tables" => sbr_tables::check(),
        "gen-ps-tables" => ps_tables::run(),
        "check-ps-tables" => ps_tables::check(),
        "gen-vorbis-tables" => vorbis_tables::run(),
        "check-vorbis-tables" => vorbis_tables::check(),
        "gen-opus-tables" => opus_tables::run(),
        "check-opus-tables" => opus_tables::check(),
        "gen-silk-tables" => silk_tables::run(),
        "check-silk-tables" => silk_tables::check(),
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
    // The checked-in generated tables must match their generators.
    resample_tables::check()?;
    filterbank_tables::check()?;
    sbr_tables::check()?;
    ps_tables::check()?;
    vorbis_tables::check()?;
    opus_tables::check()?;
    silk_tables::check()?;
    run(&["test"])?;
    // The Vorbis embedded profile (design spec "Profiles" / Task 11) is a
    // separate const configuration (stereo + 4096 max block) from the default
    // desktop build `test` just exercised — run its host tests too, so the
    // profile-cap rejection path (`new_rejects_blocksize_over_the_profile_cap`)
    // and every other Vorbis test actually execute under it, not just compile.
    run(&[
        "test",
        "-p",
        "soul-audio-core",
        "--features",
        "vorbis-embedded-profile",
    ])?;
    // The STM32 discipline gates: the core, engine, and queue must build no_std
    // for the target, and the firmware must link for it.
    run(&[
        "build",
        "-p",
        "soul-audio-core",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
    // The .soulidx art.blob reader (DAP ArtStore) must also cross-build no_std —
    // its heavy `image`-based encoder/writer stays behind the `build` feature,
    // off by default, so this proves the no_std surface never pulls it in.
    run(&[
        "build",
        "-p",
        "soul-audio-artbake",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
    // The arena-generic Vorbis codec must also cross-build under the embedded
    // profile (smaller channel/blocksize caps) — the const configuration a
    // future firmware build (issue #32) will actually use once it wires an
    // arena.
    run(&[
        "build",
        "-p",
        "soul-audio-core",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
        "--features",
        "vorbis-embedded-profile",
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
        "soul-audio-queue",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
    run(&[
        "build",
        "-p",
        "soul-audio-control",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
    run(&[
        "build",
        "-p",
        "soul-audio-tags",
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

/// Manual **by-ear QA**: play every `testdata/` fixture on the default output
/// device, then a gapless queue. The rendered signal is asserted automatically
/// by the test suite; this is for the one thing tests can't check — that it
/// actually *sounds* right through the real device. Never part of `verify`.
fn qa_audio() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("no workspace root")?;
    let fixture = |name: &str| {
        root.join("testdata")
            .join(name)
            .to_string_lossy()
            .into_owned()
    };

    println!("── QA by ear ──────────────────────────────────────────────");
    println!("The same 2 s signal is in every format:");
    println!("  A4 (440 Hz) on the LEFT, E5 (659 Hz) on the RIGHT, -6 dB, 50 ms fades.");
    println!("Listen for: identical across formats, clean (no buzz/aliasing),");
    println!("correct channels (A left / E right), and no clicks at the edges.");
    println!("Playing at 44.1 kHz through your (likely 48 kHz) device also tests the resampler.\n");

    // Build the playback binary once so its build output doesn't interleave.
    run(&[
        "build",
        "-q",
        "-p",
        "soul-audio-cli",
        "--features",
        "playback",
    ])?;

    for (label, name, hint) in [
        ("WAV", "tone.wav", "the reference"),
        ("FLAC", "tone.flac", "should sound identical to the WAV"),
        ("MP3", "tone.mp3", "lossy — a hair different is expected"),
        ("Ogg-FLAC", "tone.ogg", "should sound identical to the WAV"),
        (
            "ALAC (.m4a)",
            "tone.m4a",
            "Apple Lossless — should sound identical to the WAV",
        ),
        (
            "AAC-LC (.m4a)",
            "tone-aac.m4a",
            "AAC-LC — should sound like the WAV",
        ),
        ("AAC (ADTS)", "tone.aac", "raw AAC stream"),
        (
            "Vorbis (.ogg)",
            "tone-vorbis.ogg",
            "libvorbis — should sound like the WAV (Vorbis is dispatched by soul-audio-desktop, not AnyDecoder)",
        ),
        (
            "Opus (.opus)",
            "tone.opus",
            "libopus (CELT) — should sound like the WAV (RFC 7845 pre-skip/end-trim applied)",
        ),
        (
            "DSD (DSF)",
            "tone.dsf",
            "1-bit DSD64 decimated to PCM — same chord, faint hiss OK",
        ),
    ] {
        println!("\n=== {label} ({name}) — {hint} ===");
        play_fixtures(&[fixture(name)])?;
    }

    println!("\n=== gapless: WAV → FLAC — should be ONE continuous chord, no gap or click ===");
    play_fixtures(&[fixture("tone.wav"), fixture("tone.flac")])?;

    println!("\n✓ QA-by-ear complete.");
    Ok(())
}

/// Play files through the desktop `play` command (needs the `playback` feature).
fn play_fixtures(files: &[String]) -> Result<(), String> {
    let mut args: Vec<String> = [
        "run",
        "-q",
        "-p",
        "soul-audio-cli",
        "--features",
        "playback",
        "--",
        "play",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    args.extend(files.iter().cloned());
    println!("+ cargo {}", args.join(" "));
    let status = Command::new(env!("CARGO"))
        .args(&args)
        .status()
        .map_err(|e| format!("spawning cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("playback failed ({status})"))
    }
}
