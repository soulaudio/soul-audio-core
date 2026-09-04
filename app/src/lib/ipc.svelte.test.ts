// Tier-2: ipc.ts wrapper tests over the mocked Tauri IPC bridge.
//
// `.svelte.test.ts` (not plain `.test.ts`) so this runs in the browser
// (`client`) Vitest project: `mockIPC` patches `window.__TAURI_INTERNALS__`,
// which needs a real `window` — the `node` project's jsdom-less environment
// has none.
import { afterEach, expect, test } from "vitest";
import { mockIPC, clearMocks } from "@tauri-apps/api/mocks";
import * as ipc from "./ipc";
import type { TrackView } from "./bindings";

afterEach(() => clearMocks());

test("trackArt returns the backend data URL", async () => {
	mockIPC((cmd, payload) => {
		if (cmd === "track_art") {
			expect(payload).toEqual({ id: 7 });
			return "data:image/png;base64,AAAA";
		}
	});
	await expect(ipc.trackArt(7)).resolves.toBe("data:image/png;base64,AAAA");
});

test("trackArt resolves null when the backend has no art (not an error)", async () => {
	mockIPC((cmd) => {
		if (cmd === "track_art") return null;
	});
	await expect(ipc.trackArt(1)).resolves.toBeNull();
});

// `open_folder`'s "no folder selected" is the cancel sentinel (see
// `commands.rs`): the user closed the native dialog without picking a
// folder. `ipc.openFolder()` maps only that exact message to `null` so the
// UI can treat cancel as a no-op instead of an error toast.
test("openFolder resolves null when the dialog is cancelled", async () => {
	mockIPC((cmd) => {
		if (cmd === "open_folder") throw "no folder selected";
	});
	await expect(ipc.openFolder()).resolves.toBeNull();
});

// Any other backend error (scan failure, empty folder, …) must still
// surface — only the cancel sentinel is swallowed.
test("openFolder rejects when the scan fails", async () => {
	mockIPC((cmd) => {
		if (cmd === "open_folder") throw "scan failed: x";
	});
	await expect(ipc.openFolder()).rejects.toThrow("scan failed: x");
});

test("openFolder resolves the scanned library views on success", async () => {
	const views: TrackView[] = [
		{
			id: 1,
			title: "Track",
			artist: null,
			album: null,
			duration_secs: 120,
			has_art: false,
		},
	];
	mockIPC((cmd) => {
		if (cmd === "open_folder") return views;
	});
	await expect(ipc.openFolder()).resolves.toEqual(views);
});

test("sendCommand forwards a unit variant as the bare string literal", async () => {
	const seen: { cmd: string; payload: unknown }[] = [];
	mockIPC((cmd, payload) => {
		seen.push({ cmd, payload });
	});
	await ipc.sendCommand("TogglePlay");
	expect(seen).toContainEqual({
		cmd: "send_command",
		payload: { cmd: "TogglePlay" },
	});
});

test("sendCommand forwards a data variant as its tagged object shape", async () => {
	const seen: unknown[] = [];
	mockIPC((cmd, payload) => {
		if (cmd === "send_command") seen.push(payload);
	});
	await ipc.sendCommand({ Seek: { secs: 42 } });
	expect(seen).toContainEqual({ cmd: { Seek: { secs: 42 } } });
});

test("sendCommand rejects when the actor reports an error", async () => {
	mockIPC((cmd) => {
		if (cmd === "send_command") throw "actor gone";
	});
	await expect(ipc.sendCommand("Play")).rejects.toThrow("actor gone");
});
