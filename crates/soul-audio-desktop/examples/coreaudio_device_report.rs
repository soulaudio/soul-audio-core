//! Dump every CoreAudio output device as a **device-profile fixture** — the exact JSON the
//! fake HAL loads in `tests/exclusive_engine_contract.rs`. Ask a user with an unusual DAC /
//! Mac to run this and attach the output; dropping the file into
//! `tests/fixtures/coreaudio/` turns their hardware into a permanent regression test.
//!
//! ```text
//! cargo run -p soul-audio-desktop --features coreaudio-exclusive \
//!     --example coreaudio_device_report -- [--probe-hog] [--out DIR]
//! ```
//!
//! Read-only by default. `--probe-hog` briefly takes and releases hog mode on each device
//! to tell "supported" from "refused" (it silences other apps on that device for a moment).
//! Transport types come from `system_profiler SPAudioDataType` (no extra FFI).

#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
fn main() {
    use soul_audio_desktop::exclusive_engine::{HOG_FREE, Hal};
    use soul_audio_desktop::{CoreAudioHal, coreaudio_output_device_names, nominal_rate};

    let args: Vec<String> = std::env::args().collect();
    let probe_hog = args.iter().any(|a| a == "--probe-hog");
    let out_dir = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let transports = transports();
    let host = host_description();
    let hal = CoreAudioHal;

    for name in coreaudio_output_device_names() {
        let Some(dev) = hal.resolve(Some(&name)) else {
            continue;
        };
        let formats = hal.physical_formats(dev).unwrap_or_default();
        let hog = match hal.hog_owner(dev) {
            Err(_) => "absent",
            Ok(owner) if probe_hog && owner == HOG_FREE => match hal.toggle_hog(dev) {
                Ok(p) if p == hal.pid() => {
                    let _ = hal.toggle_hog(dev);
                    "supported"
                }
                Ok(_) => "refused",
                Err(_) => "absent",
            },
            Ok(_) => "supported", // property present; refusal not probed
        };
        let transport = transports
            .iter()
            .find(|(n, _)| n == &name)
            .map(|(_, t)| t.as_str())
            .unwrap_or("unknown");
        let id = slug(&name);
        let mut json = String::new();
        json.push_str("{\n");
        json.push_str(&format!("  \"id\": {},\n", q(&id)));
        json.push_str("  \"source\": \"captured\",\n");
        json.push_str(&format!(
            "  \"captured_on\": {},\n",
            q(&format!("{name} on {host}"))
        ));
        json.push_str(&format!("  \"name\": {},\n", q(&name)));
        json.push_str(&format!("  \"transport\": {},\n", q(transport)));
        json.push_str(&format!(
            "  \"nominal_rate\": {},\n",
            nominal_rate(Some(&name)).unwrap_or(0)
        ));
        json.push_str(&format!("  \"hog\": \"{hog}\",\n"));
        json.push_str(&format!(
            "  \"hog_probed\": {probe_hog},\n  \"fixed_rate\": {},\n",
            formats.iter().all(|f| f.rate_min == formats[0].rate_min)
                && formats.iter().all(|f| f.rate_max == f.rate_min)
        ));
        json.push_str("  \"formats\": [\n");
        let rows: Vec<String> = formats
            .iter()
            .map(|f| {
                format!(
                    "    {{ \"rate_min\": {}, \"rate_max\": {}, \"float\": {}, \"bits\": {}, \"channels\": {} }}",
                    f.rate_min, f.rate_max, f.float, f.bits, f.channels
                )
            })
            .collect();
        json.push_str(&rows.join(",\n"));
        json.push_str("\n  ]\n}\n");

        match &out_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir).expect("create --out dir");
                let path = std::path::Path::new(dir).join(format!("{id}.json"));
                std::fs::write(&path, &json).expect("write fixture");
                eprintln!("wrote {}", path.display());
            }
            None => print!("{json}"),
        }
    }
}

/// `(device name, transport)` from `system_profiler -json SPAudioDataType`.
#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
fn transports() -> Vec<(String, String)> {
    let Ok(out) = std::process::Command::new("system_profiler")
        .args(["-json", "SPAudioDataType"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // Minimal scrape (no JSON dep in the library): pair each "_name" with the next
    // "coreaudio_device_transport" inside the same item.
    let mut v = Vec::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        let l = line.trim();
        if let Some(n) = field(l, "\"_name\"") {
            name = Some(n);
        } else if let Some(t) = field(l, "\"coreaudio_device_transport\"")
            && let Some(n) = name.take()
        {
            let t = t
                .trim_start_matches("coreaudio_device_type_")
                .replace('_', "-");
            v.push((n, t));
        }
    }
    v
}

#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
fn field(line: &str, key: &str) -> Option<String> {
    let rest = line
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix(':')?
        .trim();
    Some(rest.trim_end_matches(',').trim_matches('"').to_string())
}

#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
fn host_description() -> String {
    let sh = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    format!(
        "{} ({}), macOS {}",
        sh("sysctl", &["-n", "hw.model"]),
        sh("uname", &["-m"]),
        sh("sw_vers", &["-productVersion"])
    )
}

#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(not(all(target_os = "macos", feature = "coreaudio-exclusive")))]
fn main() {
    eprintln!("coreaudio_device_report needs macOS and --features coreaudio-exclusive");
}
