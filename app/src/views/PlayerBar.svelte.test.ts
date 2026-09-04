// Tier-2: PlayerBar's seek-hold behavior — the "seek jumps back" fix.
// `.svelte.test.ts` so it runs in the browser (`client`) project (real
// Chromium DOM + faithful Svelte 5 rune reactivity).
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { render } from "vitest-browser-svelte";
import { tick } from "svelte";
import { mockIPC, clearMocks } from "@tauri-apps/api/mocks";
import PlayerBar from "./PlayerBar.svelte";
import { player } from "../lib/store.svelte";
import type { TrackView } from "../lib/bindings";

const library: TrackView[] = [
	{
		id: 1,
		title: "One",
		artist: "A",
		album: "Alpha",
		duration_secs: 200,
		has_art: false,
	},
	{
		id: 2,
		title: "Two",
		artist: "B",
		album: "Beta",
		duration_secs: 100,
		has_art: false,
	},
];

function seekInput(container: HTMLElement): HTMLInputElement {
	const el = container.querySelector("input.seek");
	if (!el) throw new Error("seek input not found");
	return el as HTMLInputElement;
}

function fireInput(el: HTMLInputElement, value: number): void {
	el.value = String(value);
	el.dispatchEvent(new Event("input", { bubbles: true }));
}

function fireChange(el: HTMLInputElement, value: number): void {
	el.value = String(value);
	el.dispatchEvent(new Event("change", { bubbles: true }));
}

beforeEach(() => {
	// track_art / send_command both fire during these tests (art effect on
	// mount + on track change; seek() sends a Seek command on release) — mock
	// them so those fire-and-forget calls don't reject with "no
	// __TAURI_INTERNALS__" and surface as unhandled rejections.
	mockIPC((cmd) => {
		if (cmd === "track_art") return null;
		if (cmd === "send_command") return null;
	});
	player.library = library;
	player.nowPlayingId = 1;
	player.positionSecs = 10;
	player.durationSecs = 200;
	player.playing = false;
	player.volume = 1;
});

afterEach(() => {
	clearMocks();
	vi.useRealTimers();
});

test("scrubbing and releasing holds the displayed position through the ~300ms window", async () => {
	vi.useFakeTimers();
	const screen = await render(PlayerBar);
	const seek = seekInput(screen.container);

	// Drag to 55s.
	fireInput(seek, 55);
	await tick();
	expect(seek.value).toBe("55");

	// Release: seekHold takes over and the slider stays at the target even
	// though the live position (below) is still stale at 10.
	fireChange(seek, 55);
	await tick();
	expect(seek.value).toBe("55");

	// A stale Tick lands mid-hold (the actor hasn't caught up to the seek
	// yet) — the hold must win, not the tick. Use a value different from the
	// `beforeEach` default (10) so this step actually changes something —
	// otherwise the assertion below would pass even if the hold were broken.
	player.positionSecs = 12;
	await tick();
	await vi.advanceTimersByTimeAsync(150);
	await tick();
	expect(seek.value).toBe("55");

	// Past the ~300ms hold window: falls back to the live position (12, set
	// above, proving the fallback reads the current position and not some
	// cached/stale value).
	await vi.advanceTimersByTimeAsync(200);
	await tick();
	expect(seek.value).toBe("12");
});

test("seekHold clears immediately when the now-playing track changes", async () => {
	vi.useFakeTimers();
	const screen = await render(PlayerBar);
	const seek = seekInput(screen.container);

	fireInput(seek, 55);
	fireChange(seek, 55);
	await tick();
	expect(seek.value).toBe("55");

	// Track change mid-hold: the held position is meaningless for the new
	// track, so it must clear at once (not wait out the 300ms timer).
	player.nowPlayingId = 2;
	player.positionSecs = 3;
	player.durationSecs = 100;
	await tick();
	expect(seek.value).toBe("3");

	// The old hold's timeout firing later must not resurrect a stale value.
	await vi.advanceTimersByTimeAsync(400);
	await tick();
	expect(seek.value).toBe("3");
});
