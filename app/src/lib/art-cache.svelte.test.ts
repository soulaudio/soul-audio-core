// Tier-2: art-cache.svelte.ts — album-keyed de-dup + the `inView` visibility
// gate. `.svelte.test.ts` so it runs in the browser (`client`) project (real
// `IntersectionObserver`, though we stub it below for determinism — see note
// on the `inView` test).
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mockIPC, clearMocks } from "@tauri-apps/api/mocks";
import { albumKey, artCache, ensureArt, inView } from "./art-cache.svelte";
import { FakeIntersectionObserver } from "./__fixtures__/intersection-observer";

afterEach(() => {
	clearMocks();
	artCache.clear();
});

test("albumKey keys by album when present, else the track's own id", () => {
	expect(albumKey({ id: 1, album: "Foo" })).toBe("Foo");
	expect(albumKey({ id: 2, album: null })).toBe("2");
});

test("ensureArt de-dupes repeat requests for the same key (in-flight and cached)", async () => {
	let calls = 0;
	mockIPC((cmd) => {
		if (cmd === "track_art") {
			calls++;
			return "data:image/png;base64,AAAA";
		}
	});

	// Two calls before the first fetch resolves: the second must not re-invoke
	// (de-duped via the `inFlight` set).
	ensureArt({ id: 1, album: "Foo" });
	ensureArt({ id: 1, album: "Foo" });

	await vi.waitFor(() =>
		expect(artCache.get("Foo")).toBe("data:image/png;base64,AAAA"),
	);
	expect(calls).toBe(1);

	// A third call once the entry is cached must also not re-invoke.
	ensureArt({ id: 1, album: "Foo" });
	await vi.waitFor(() => expect(calls).toBe(1));
});

// The `inView` Svelte action defers the fetch until the node's
// `IntersectionObserver` reports it on-screen. Real Chromium's observer is
// asynchronous and timed to actual layout/scroll — stubbing it with a
// synchronous, manually-triggered fake (see `./__fixtures__/intersection-observer`)
// makes the "gated until visible" assertion deterministic instead of
// depending on real viewport geometry.
beforeEach(() => {
	FakeIntersectionObserver.instances = [];
	vi.stubGlobal("IntersectionObserver", FakeIntersectionObserver);
});

afterEach(() => {
	vi.unstubAllGlobals();
});

test("inView gates the fetch until the node reports as intersecting", async () => {
	let calls = 0;
	mockIPC((cmd) => {
		if (cmd === "track_art") {
			calls++;
			return "data:image/png;base64,BBBB";
		}
	});

	const node = document.createElement("div");
	const action = inView(node, { id: 5, album: "Bar" });
	const observer = FakeIntersectionObserver.instances.at(-1);
	expect(observer).toBeDefined();

	// Not yet intersecting: no fetch.
	observer?.fire(false);
	expect(calls).toBe(0);
	expect(artCache.has("Bar")).toBe(false);

	// Now intersecting: fetch fires, and the observer stops watching (a
	// single fetch per row — later scroll in/out never re-fetches).
	observer?.fire(true);
	await vi.waitFor(() =>
		expect(artCache.get("Bar")).toBe("data:image/png;base64,BBBB"),
	);
	expect(calls).toBe(1);
	expect(observer?.disconnected).toBe(true);

	action.destroy();
});
