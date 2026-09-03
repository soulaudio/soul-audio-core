<script lang="ts">
  // Root layout: left library list, bottom transport bar, top-right queue.
  import { onMount } from "svelte";
  import { bindPlayerEvents } from "./lib/store.svelte";
  import Library from "./views/Library.svelte";
  import PlayerBar from "./views/PlayerBar.svelte";
  import Queue from "./views/Queue.svelte";

  let ready = $state(false);

  onMount(() => {
    // Subscribe to the actor's event stream (also loads an already-open
    // library). The returned fn unsubscribes on unmount.
    let teardown: (() => void) | undefined;
    void bindPlayerEvents().then((fn) => {
      teardown = fn;
      ready = true;
    });
    return () => teardown?.();
  });
</script>

{#if !ready}
  <p class="boot">soul-player…</p>
{:else}
  <div class="app">
    <main class="left">
      <Library />
    </main>
    <aside class="right">
      <Queue />
    </aside>
    <footer class="player">
      <PlayerBar />
    </footer>
  </div>
{/if}

<style>
  .boot {
    height: 100vh;
    display: grid;
    place-items: center;
    color: var(--fg-dim);
    font-size: 0.9rem;
    margin: 0;
  }
  .app {
    display: grid;
    grid-template-rows: 1fr auto;
    grid-template-columns: minmax(0, 1fr) minmax(0, 18rem);
    grid-template-areas:
      "left right"
      "player player";
    height: 100vh;
  }
  .left {
    grid-area: left;
    min-width: 0;
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-right: 1px solid color-mix(in srgb, var(--fg) 10%, transparent);
  }
  .right {
    grid-area: right;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .player {
    grid-area: player;
    border-top: 1px solid color-mix(in srgb, var(--fg) 12%, transparent);
    background: color-mix(in srgb, var(--fg) 2%, transparent);
  }
</style>
