// Tier-2: Library's row rendering + lazy art-fetch gating.
// `.svelte.test.ts` so it runs in the browser (`client`) project.
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { render } from "vitest-browser-svelte";
import { tick } from "svelte";
import { mockIPC, clearMocks } from "@tauri-apps/api/mocks";
import Library from "./Library.svelte";
import { player } from "../lib/store.svelte";
import { artCache } from "../lib/art-cache.svelte";
import type { TrackView } from "../lib/bindings";
import { FakeIntersectionObserver } from "../lib/__fixtures__/intersection-observer";

const tracks: TrackView[] = [
	{
		id: 1,
		title: "One",
		artist: "A",
		album: "Alpha",
		duration_secs: 100,
	},
	{
		id: 2,
		title: "Two",
		artist: "B",
		album: "Beta",
		duration_secs: 120,
	},
	{
		id: 3,
		title: "Three",
		artist: "C",
		album: null,
		duration_secs: 90,
	},
];

// Real Chromium's `IntersectionObserver` fires based on actual layout/scroll
// timing, which would make "does this row's art get requested" nondeterministic
// in a headless run. Stub it (see `../lib/__fixtures__/intersection-observer`,
// the same fake used directly against the `inView` action in
// art-cache.svelte.test.ts) so each row's visibility is under direct,
// synchronous test control.
beforeEach(() => {
	FakeIntersectionObserver.instances = [];
	vi.stubGlobal("IntersectionObserver", FakeIntersectionObserver);
	artCache.clear();
	player.library = tracks;
	player.nowPlayingId = null;
});

afterEach(() => {
	clearMocks();
	vi.unstubAllGlobals();
});

test("renders one row per library track", async () => {
	mockIPC(() => null);
	const screen = await render(Library);
	const rows = screen.container.querySelectorAll("ol.tracks > li");
	expect(rows).toHaveLength(tracks.length);
});

// Cover art is fetched per row only when the row scrolls into view — the gate
// is row *visibility* (`inView`/`IntersectionObserver`), nothing else. (There
// is no scan-time art flag to gate on: `has_art` was dropped as unused and
// misleading in issue #33, since cover resolution also tries an external
// "Scans/" file, so every visible row requests art and lets the resolver
// decide.) This test documents that: a row not yet scrolled into view is never
// fetched; once visible, it is.
test("art is requested only for rows that have scrolled into view", async () => {
	const seenIds: number[] = [];
	mockIPC((cmd, payload) => {
		if (cmd === "track_art") {
			seenIds.push((payload as { id: number }).id);
			return null;
		}
	});

	await render(Library);
	expect(FakeIntersectionObserver.instances).toHaveLength(tracks.length);

	// Track #3 (own album key) never intersects: no request.
	// Tracks #1 and #2 both scroll into view.
	FakeIntersectionObserver.instances[0]?.fire(true); // track 1
	FakeIntersectionObserver.instances[1]?.fire(true); // track 2
	await tick();

	await vi.waitFor(() => expect(seenIds).toContain(1));
	await vi.waitFor(() => expect(seenIds).toContain(2));
	expect(seenIds).not.toContain(3);
});
