// Lazy, per-album cover-art cache for the library list.
//
// Rows only fetch art once they scroll into view (see `inView` below), and
// fetches are de-duplicated by album: tracks that share an album share one
// `trackArt` call and one cache entry, instead of fetching the same cover
// once per track.
import { SvelteMap } from "svelte/reactivity";
import { trackArt } from "./ipc";
import type { TrackView } from "./bindings";

type ArtKeyed = Pick<TrackView, "id" | "album">;

/** Cache key: tracks sharing an album share one entry; albumless tracks key
 *  by their own id so they never collide with one another. */
export function albumKey(track: ArtKeyed): string {
	return track.album ?? String(track.id);
}

// `SvelteMap` (unlike a plain `$state(new Map())`) makes `.get`/`.set`/`.has`
// themselves reactive, so a component reading `artCache.get(key)` re-renders
// as soon as that key's fetch resolves — no manual version counter needed.
export const artCache = new SvelteMap<string, string | null>();

// Keys with a fetch in flight. Plain `Set`: it's only ever read inside
// `ensureArt` below, never rendered, so it doesn't need to be reactive.
const inFlight = new Set<string>();

/** Kick off a cover-art fetch for `track`'s album key, unless that key is
 *  already cached or already loading. Fire-and-forget; the result lands in
 *  `artCache` (reactively) whenever it resolves.
 *
 *  NOTE: cover files can be multi-MB (external "Scans/" folders etc.) — the
 *  browser downscales the decoded image for the ~36px thumbnail, but the
 *  full file still crosses IPC. Server-side downscaling is a possible future
 *  optimization; not done here. */
export function ensureArt(track: ArtKeyed): void {
	const key = albumKey(track);
	if (artCache.has(key) || inFlight.has(key)) return;
	inFlight.add(key);
	trackArt(track.id)
		.then((url) => {
			artCache.set(key, url);
		})
		.finally(() => {
			inFlight.delete(key);
		});
}

/** Svelte action: the first time `node` scrolls into view, resolves cover
 *  art for `track` (via `ensureArt`) and then stops observing — later
 *  scrolls in/out never re-fetch. `rootMargin` starts the fetch a little
 *  before the row is actually on screen. */
export function inView(node: Element, track: ArtKeyed) {
	let current = track;
	let fired = false;
	const observer = new IntersectionObserver(
		(entries) => {
			if (fired) return;
			for (const entry of entries) {
				if (entry.isIntersecting) {
					fired = true;
					ensureArt(current);
					observer.disconnect();
					break;
				}
			}
		},
		{ rootMargin: "200px" },
	);
	observer.observe(node);
	return {
		update(next: ArtKeyed) {
			current = next;
		},
		destroy() {
			observer.disconnect();
		},
	};
}
