fn main() {
    tauri_build::build();

    // Windows/MSVC-only workaround for a known Tauri bug: `tauri-build`'s own
    // manifest embed (Common Controls v6) is scoped to the `[[bin]]` target,
    // so `cargo test` binaries load the legacy v5 comctl32.dll — which is
    // missing v6-only exports (TaskDialogIndirect, *WindowSubclass) that
    // tauri-runtime-wry's mock runtime imports — and crash at process launch
    // with STATUS_ENTRYPOINT_NOT_FOUND before any test code runs. Embedding
    // the same manifest into test binaries only (via `-tests`, never the
    // unqualified/`-bin` form, which would double-embed against tauri-build's
    // own bin manifest and fail to link with a "duplicate resource" error)
    // fixes it. See https://github.com/tauri-apps/tauri/issues/13419 and
    // https://github.com/tauri-apps/tauri/pull/4383#issuecomment-1212221864.
    #[cfg(windows)]
    {
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
        let target_env = std::env::var("CARGO_CFG_TARGET_ENV");
        if target_os == "windows" && target_env.as_deref() == Ok("msvc") {
            let manifest =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("windows-app-manifest.xml");
            println!("cargo:rerun-if-changed={}", manifest.display());
            println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
            println!(
                "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
                manifest.display()
            );
        }
    }
}
