<script lang="ts">
  import { openFolder } from "../lib/ipc";
  import { player, playFrom, enqueue, playNext } from "../lib/store.svelte";
  import type { TrackView } from "../lib/bindings";

  let error = $state<string | null>(null);
  let busy = $state(false);

  function fmt(secs: number | null): string {
    if (secs == null || !Number.isFinite(secs)) return "–";
    const m = Math.floor(secs / 60);
    const s = Math.floor(secs % 60);
    return `${m}:${s.toString().padStart(2, "0")}`;
  }

  async function pickFolder() {
    busy = true;
    error = null;
    try {
      const views = await openFolder();
      if (views === null) return; // cancelled
      // The scan result is the initial library; feed it into the store so the
      // list renders (thereafter Tick/Queue events keep now-playing/queue in
      // sync — they never carry the library itself).
      player.library = views;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  // Row actions: left-click plays the track; hover "next/queue" buttons add
  // it without interrupting playback.
  function rowMeta(t: TrackView): string {
    return [t.artist, t.album].filter(Boolean).join(" · ");
  }
</script>

<section class="library">
  <header class="lib-header">
    <h2>Library</h2>
    <button class="ghost" onclick={pickFolder} disabled={busy}>
      {busy ? "Scanning…" : "Open folder…"}
    </button>
  </header>

  {#if error}
    <p class="error">{error}</p>
  {/if}

  {#if player.library.length === 0}
    <p class="empty">No library yet — open a folder of music to begin.</p>
  {:else}
    <ol class="tracks">
      {#each player.library as track, i (track.id)}
        {@const playingThis = player.nowPlayingId === track.id}
        <li class:current={playingThis}>
          <button
            type="button"
            class="rowbtn"
            onclick={() => playFrom(i)}
            aria-current={playingThis ? "true" : undefined}
          >
            <span class="idx">{playingThis ? "▶" : i + 1}</span>
            <span class="meta">
              <span class="title">{track.title ?? "Untitled"}</span>
              <span class="sub">{rowMeta(track)}</span>
            </span>
            <span class="dur">{fmt(track.duration_secs)}</span>
          </button>
          <span class="actions">
            <button
              type="button"
              class="mini"
              title="Play next"
              onclick={() => playNext(track.id)}
            >
              ▶
            </button>
            <button
              type="button"
              class="mini"
              title="Add to queue"
              onclick={() => enqueue(track.id)}
            >
              +
            </button>
          </span>
        </li>
      {/each}
    </ol>
  {/if}
</section>

<style>
  .library {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 1;
  }
  .lib-header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    padding: 0.5rem 0.75rem;
    border-bottom: 1px solid color-mix(in srgb, var(--fg) 12%, transparent);
  }
  h2 {
    margin: 0;
    font-size: 0.95rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--fg-dim);
  }
  .ghost {
    background: none;
    border: 1px solid color-mix(in srgb, var(--fg) 25%, transparent);
    color: var(--fg);
    border-radius: 6px;
    padding: 0.25rem 0.6rem;
    font-size: 0.8rem;
    cursor: pointer;
  }
  .ghost:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  .ghost:disabled {
    opacity: 0.5;
    cursor: wait;
  }
  .empty {
    margin: auto;
    color: var(--fg-dim);
    font-size: 0.9rem;
    padding: 1rem;
    text-align: center;
  }
  .error {
    color: #e06c75;
    font-size: 0.8rem;
    margin: 0.5rem 0.75rem;
  }
  .tracks {
    list-style: none;
    margin: 0;
    padding: 0.25rem 0;
    overflow-y: auto;
  }
  .tracks li {
    display: flex;
    align-items: center;
    gap: 0.2rem;
    padding: 0 0.75rem 0 0;
    border-left: 2px solid transparent;
  }
  .tracks li:hover {
    background: color-mix(in srgb, var(--fg) 6%, transparent);
  }
  .tracks li:has(.rowbtn:focus-visible) {
    background: color-mix(in srgb, var(--fg) 6%, transparent);
  }
  .tracks li.current {
    border-left-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 10%, transparent);
  }
  .rowbtn {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex: 1;
    min-width: 0;
    padding: 0.3rem 0.15rem 0.3rem 0.6rem;
    background: none;
    border: none;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
    border-radius: 4px;
  }
  .rowbtn:hover {
    background: color-mix(in srgb, var(--fg) 5%, transparent);
  }
  .rowbtn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .tracks li.current .rowbtn .idx {
    color: var(--accent);
  }
  .idx {
    width: 1.6rem;
    text-align: right;
    color: var(--fg-dim);
    font-variant-numeric: tabular-nums;
    font-size: 0.85rem;
  }
  .meta {
    display: flex;
    flex-direction: column;
    min-width: 0;
    flex: 1;
  }
  .title {
    font-size: 0.9rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .sub {
    font-size: 0.75rem;
    color: var(--fg-dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .dur {
    color: var(--fg-dim);
    font-size: 0.75rem;
    font-variant-numeric: tabular-nums;
  }
  .actions {
    opacity: 0;
  }
  .tracks li:hover .actions,
  .tracks li:focus-within .actions {
    opacity: 1;
  }
  .mini {
    background: none;
    border: 1px solid color-mix(in srgb, var(--fg) 30%, transparent);
    color: var(--fg);
    border-radius: 4px;
    font-size: 0.7rem;
    padding: 0.1rem 0.35rem;
    cursor: pointer;
  }
  .mini:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
</style>
