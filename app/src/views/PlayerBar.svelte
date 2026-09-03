<script lang="ts">
  import {
    player,
    togglePlay,
    next,
    prev,
    stop,
    seek,
    setVolume,
    cycleRepeat,
    toggleShuffle,
  } from "../lib/store.svelte";
  import { trackById } from "../lib/store.svelte";
  import { trackArt } from "../lib/ipc";

  let scrubbing = $state(false);

  function fmt(secs: number): string {
    if (!Number.isFinite(secs)) return "0:00";
    const m = Math.floor(secs / 60);
    const s = Math.floor(secs % 60);
    return `${m}:${s.toString().padStart(2, "0")}`;
  }

  // While the user drags, keep the slider local and only issue the seek on
  // release (the ~8 Hz tick would fight the drag otherwise).
  let dragPos = $state(0);
  // After releasing a seek, hold the slider at the target until the post-seek
  // position tick propagates (~125 ms). Without this the slider snaps to the
  // stale pre-seek `positionSecs` for a frame and then jumps to the target — a
  // visible back-and-forth flicker.
  let seekHold = $state<number | null>(null);
  function sliderPos(): number {
    if (scrubbing) return dragPos;
    if (seekHold !== null) return seekHold;
    return player.positionSecs;
  }
  function onInput(e: Event): void {
    dragPos = Number((e.target as HTMLInputElement).value);
    scrubbing = true;
  }
  function onChange(e: Event): void {
    const target = Number((e.target as HTMLInputElement).value);
    seek(target);
    scrubbing = false;
    seekHold = target;
    setTimeout(() => {
      if (seekHold === target) seekHold = null;
    }, 300);
  }
  // A track change makes a held seek position meaningless — resume live at once.
  $effect(() => {
    void player.nowPlayingId;
    seekHold = null;
  });
  function onVolume(e: Event): void {
    setVolume(Number((e.target as HTMLInputElement).value));
  }

  const repeatLabel: Record<string, string> = {
    Off: "Repeat: Off",
    All: "Repeat: All",
    One: "Repeat: One",
  };
  const current = $derived(
    player.nowPlayingId === null ? null : trackById(player.nowPlayingId),
  );
  const repeatIcon = $derived(player.repeat === "Off" ? "↻" : player.repeat === "All" ? "↻" : "↻₁");

  // Now-playing cover art: fetched on demand for every track (the backend
  // resolves embedded art AND an external cover file, so we can't gate on
  // `has_art`, which only reflects embedded art). `track_art` returns a
  // ready-to-use `data:` URL, so there's no Blob/object-URL lifecycle to manage.
  // Re-runs whenever `current` changes (track switch or a library reload).
  let artUrl = $state<string | null>(null);

  $effect(() => {
    const id = current?.id ?? null;

    // Clear immediately so a stale cover never lingers while the new one loads.
    artUrl = null;
    if (id === null) {
      return;
    }

    let cancelled = false;
    trackArt(id).then((url) => {
      if (!cancelled) artUrl = url;
    });

    return () => {
      cancelled = true;
    };
  });
</script>

<div class="now">
  <div class="np-left">
    <div class="art">
      {#if artUrl}
        <img src={artUrl} alt={current?.album ?? "Cover art"} />
      {:else}
        <span class="art-placeholder" aria-hidden="true">♪</span>
      {/if}
    </div>
    <div class="track-info">
      <span class="np-title">{current?.title ?? "No track"}</span>
      <span class="np-sub">{current?.artist ?? "–"}</span>
    </div>
  </div>
  <div class="time">{fmt(sliderPos())} / {fmt(player.durationSecs)}</div>
</div>

<input
  class="seek"
  type="range"
  min="0"
  max={Math.max(player.durationSecs, 1)}
  step="0.5"
  value={sliderPos()}
  oninput={onInput}
  onchange={onChange}
  disabled={player.durationSecs === 0}
  aria-label="Seek"
/>

<div class="controls">
  <button class="icon" title="Previous" onclick={prev}>⏮</button>
  <button class="icon big" title={player.playing ? "Pause" : "Play"} onclick={togglePlay}>
    {player.playing ? "⏸" : "▶"}
  </button>
  <button class="icon" title="Next" onclick={next}>⏭</button>
  <button class="icon" title="Stop" onclick={stop}>⏹</button>

  <span class="spacer"></span>

  <button
    class="icon"
    title={repeatLabel[player.repeat]}
    onclick={cycleRepeat}
    class:on={player.repeat !== "Off"}
  >
    {repeatIcon}
  </button>
  <button
    class="icon"
    title="Shuffle"
    onclick={toggleShuffle}
    class:on={player.shuffle}
  >
    ⤨
  </button>
  <span class="vol">
    <span class="vol-icon">🔉</span>
    <input
      type="range"
      min="0"
      max="1"
      step="0.01"
      value={player.volume}
      oninput={onVolume}
      aria-label="Volume"
    />
  </span>
</div>

<style>
  .now {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    padding: 0 0.75rem;
    gap: 1rem;
  }
  .np-left {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    min-width: 0;
  }
  .art {
    flex: 0 0 auto;
    width: 48px;
    height: 48px;
    border-radius: 6px;
    overflow: hidden;
    display: flex;
    align-items: center;
    justify-content: center;
    background: color-mix(in srgb, var(--fg) 8%, transparent);
  }
  .art img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .art-placeholder {
    color: var(--fg-dim);
    font-size: 1.2rem;
  }
  .track-info {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .np-title {
    font-size: 0.95rem;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .np-sub {
    font-size: 0.75rem;
    color: var(--fg-dim);
  }
  .time {
    font-size: 0.75rem;
    color: var(--fg-dim);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .seek {
    width: calc(100% - 1.5rem);
    margin: 0.5rem 0.75rem 0.25rem;
  }
  .controls {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.25rem 0.75rem 0.75rem;
  }
  .icon {
    background: none;
    border: 1px solid transparent;
    color: var(--fg);
    font-size: 1rem;
    line-height: 1;
    padding: 0.3rem 0.45rem;
    border-radius: 6px;
    cursor: pointer;
  }
  .icon:hover {
    background: color-mix(in srgb, var(--fg) 8%, transparent);
  }
  .icon.big {
    font-size: 1.3rem;
  }
  .icon.on {
    color: var(--accent);
    border-color: color-mix(in srgb, var(--accent) 40%, transparent);
  }
  .spacer {
    flex: 1;
  }
  .vol {
    display: flex;
    align-items: center;
    gap: 0.3rem;
  }
  .vol-icon {
    font-size: 0.8rem;
    color: var(--fg-dim);
  }
  input[type="range"] {
    accent-color: var(--accent);
  }
  .vol input {
    width: 6rem;
  }
</style>
