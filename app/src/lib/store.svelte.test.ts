// Tier-2: the player store's reaction to synthetic "player" actor events.
// `.svelte.test.ts` so it runs in the browser (`client`) project — real
// `window`/WebCrypto for `@tauri-apps/api`'s `listen`/`emit`, and faithful
// Svelte 5 rune reactivity for the `$state` store.
import { afterEach, expect, test } from "vitest";
import { mockIPC, clearMocks } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import { bindPlayerEvents, player } from "./store.svelte";

afterEach(() => clearMocks());

test("a Tick event updates now-playing, position, duration and volume", async () => {
	mockIPC(
		(cmd) => {
			if (cmd === "library_views") return [];
		},
		{ shouldMockEvents: true },
	);

	const unbind = await bindPlayerEvents();
	try {
		await emit("player", {
			Tick: {
				now_playing_id: 3,
				position_secs: 12.5,
				duration_secs: 200,
				state: "Playing",
				shuffle: true,
				repeat: "All",
				volume: 0.5,
			},
		});

		expect(player.nowPlayingId).toBe(3);
		expect(player.positionSecs).toBe(12.5);
		expect(player.durationSecs).toBe(200);
		expect(player.playing).toBe(true);
		expect(player.volume).toBe(0.5);
		expect(player.shuffle).toBe(true);
		expect(player.repeat).toBe("All");
	} finally {
		unbind();
	}
});

test("a Queue event updates up-next and context position", async () => {
	mockIPC(
		(cmd) => {
			if (cmd === "library_views") return [];
		},
		{ shouldMockEvents: true },
	);

	const unbind = await bindPlayerEvents();
	try {
		await emit("player", {
			Queue: {
				up_next_ids: [5, 6, 7],
				context_pos: 2,
				context_len: 10,
			},
		});

		expect(player.upNext).toEqual([5, 6, 7]);
		expect(player.contextPos).toBe(2);
		expect(player.contextLen).toBe(10);
	} finally {
		unbind();
	}
});

test("bindPlayerEvents loads the current library views up front", async () => {
	const views = [
		{
			id: 9,
			title: "T",
			artist: null,
			album: null,
			duration_secs: 1,
			has_art: false,
		},
	];
	mockIPC(
		(cmd) => {
			if (cmd === "library_views") return views;
		},
		{ shouldMockEvents: true },
	);

	const unbind = await bindPlayerEvents();
	try {
		expect(player.library).toEqual(views);
	} finally {
		unbind();
	}
});
