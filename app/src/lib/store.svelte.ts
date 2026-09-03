// Player UI store (Svelte 5 runes — `.svelte.ts` so the compiler transforms
// `$state`). One exported `player` object: property mutation is legal TS (the
// raw TS LSP accepts it) AND reactive under runes. NOTE: don't name it `state`
// — an exported `state` binding collides with the `$state` rune in component
// files under svelte-check.
//
// Fed ONLY by the "player" event channel (Tick ~8 Hz + Queue on mutation) plus
// one-shot command results. Transport actions go back through ipc.sendCommand;
// the actor is the source of truth.
import * as ipc from "./ipc";
import type { RepeatMode, TrackView } from "./bindings";

export const player = $state<{
	library: TrackView[];
	nowPlayingId: number | null;
	positionSecs: number;
	durationSecs: number;
	playing: boolean;
	volume: number;
	shuffle: boolean;
	repeat: RepeatMode;
	upNext: number[];
	contextPos: number;
	contextLen: number;
}>({
	library: [],
	nowPlayingId: null,
	positionSecs: 0,
	durationSecs: 0,
	playing: false,
	volume: 1,
	shuffle: false,
	repeat: "Off",
	upNext: [],
	contextPos: 0,
	contextLen: 0,
});

// Track lookup helper for the queue view.
export function trackById(id: number): TrackView | undefined {
	return player.library.find((t) => t.id === id);
}

let unlisten: (() => void) | null = null;

/** Bind the actor event stream into the store. Idempotent (re-listening swaps
 *  the prior subscription). Returns an unsubscribe fn for cleanup on unmount. */
export async function bindPlayerEvents(): Promise<() => void> {
	// Load whatever library is already open (fast path after re-open).
	player.library = await ipc.libraryViews();
	if (unlisten) {
		unlisten();
	}
	unlisten = await ipc.onPlayerEvent((ev) => {
		if ("Tick" in ev && ev.Tick) {
			const t = ev.Tick;
			player.nowPlayingId = t.now_playing_id;
			player.positionSecs = t.position_secs ?? 0;
			player.durationSecs = t.duration_secs ?? 0;
			player.playing = t.state === "Playing";
			player.volume = t.volume ?? 1;
			player.shuffle = t.shuffle;
			player.repeat = t.repeat;
		} else if ("Queue" in ev && ev.Queue) {
			const q = ev.Queue;
			player.upNext = q.up_next_ids;
			player.contextPos = q.context_pos;
			player.contextLen = q.context_len;
		}
	});
	return () => {
		unlisten?.();
		unlisten = null;
	};
}

// --- Transport helpers -------------------------------------------------------
// Unit variants serialize as bare strings ("Next"); payload variants as the
// tagged object shape from bindings.ts.

export function togglePlay(): void {
	void ipc.sendCommand(player.playing ? "Pause" : "Play");
}

export function next(): void {
	void ipc.sendCommand("Next");
}

export function prev(): void {
	void ipc.sendCommand("Prev");
}

export function stop(): void {
	void ipc.sendCommand("Stop");
}

export function seek(secs: number): void {
	void ipc.sendCommand({ Seek: { secs } });
}

export function setVolume(v: number): void {
	void ipc.sendCommand({ SetVolume: v });
}

export function cycleRepeat(): void {
	const nextMode: Record<RepeatMode, RepeatMode> = {
		Off: "All",
		All: "One",
		One: "Off",
	};
	void ipc.sendCommand({ SetRepeat: nextMode[player.repeat] });
}

export function toggleShuffle(): void {
	void ipc.sendCommand({ SetShuffle: !player.shuffle });
}

export function playFrom(index: number): void {
	void ipc.sendCommand({
		PlayContext: { ids: player.library.map((t) => t.id), start: index },
	});
}

export function playNext(id: number): void {
	void ipc.sendCommand({ PlayNext: id });
}

export function enqueue(id: number): void {
	void ipc.sendCommand({ Enqueue: id });
}

export function clearQueue(): void {
	void ipc.sendCommand("ClearQueue");
}
