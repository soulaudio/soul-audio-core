# E2E suite fast-follows

Three small items the desktop-player e2e test suite (`docs/superpowers/plans/2026-09-04-desktop-e2e-test-suite.md`) surfaced but didn't fix in-branch. Logged here so they aren't lost; can be converted to Forgejo issues if desired.

## A — `openFolder()` threw on dialog cancel (FIXED in this commit)

`ipc.openFolder()` unconditionally threw on any backend error from `open_folder`, including the cancel case (`blocking_pick_folder()` returning `None`, surfaced as the sentinel string `"no folder selected"` — see `app/src-tauri/src/commands.rs`). That left a dead `if (views === null) return` branch in `Library.svelte` and popped a red error toast whenever the user simply closed the folder-picker dialog.

Fixed: `ipc.openFolder()` now maps only the exact `"no folder selected"` message to a `null` resolution; every other error (scan failure, empty folder, …) still throws. Return type is `Promise<TrackView[] | null>` with the `| null` meaningful again. `Library.svelte`'s `if (views === null) return` branch is now live and correct.

## B — `has_art` doc was misleading re: external art (DOC FIX ONLY in this commit)

`TrackView.has_art` reflects **embedded** cover art only. `track_art` (the command that actually fetches cover bytes) also resolves **external** cover files next to the track — e.g. `Scans/Front.jpg`, the common layout for DSD/SACD rips that carry only text tags. So a track with `has_art: false` can still have visible art.

The frontend intentionally fetches `track_art` for **every** visible row regardless of `has_art` — gating the fetch on `has_art` was tried before and regressed external-cover rendering. That fetch-always behavior is correct and unchanged by this follow-up; only the stale doc comment (which claimed the UI uses `has_art` to decide whether to request art) was corrected, in both `crates/soul-audio-service/src/types.rs` and the regenerated `app/src/lib/bindings.ts`.

## C — Bare `.aac` (ADTS) files report no duration / no seek bar (KNOWN LIMITATION, not fixed)

Bare ADTS-framed `.aac` files play but show no duration and no working seek bar. Root cause: the tags layer (`soul-audio-tags`) has no ADTS frame-scan path to derive duration for raw `.aac` — it only computes duration from container-level facts (e.g. MP4 `stsz`/`stts` boxes), which bare ADTS streams don't have. `.m4a` / AAC-in-MP4 files are unaffected since the MP4 container carries duration directly.

Left as a known limitation. A fix would mean adding an ADTS frame-scan (walk frames, sum `samples-per-frame / sample-rate`) to the tags crate's duration path — out of scope for this fast-follow pass.
