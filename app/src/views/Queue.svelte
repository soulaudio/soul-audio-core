<script lang="ts">
  import { player, trackById, clearQueue } from "../lib/store.svelte";

  // Format like the library rows.
  function titleFor(id: number): string {
    return trackById(id)?.title ?? `Track ${id}`;
  }
</script>

<section class="queue">
  <header class="q-header">
    <h2>Up Next</h2>
    {#if player.upNext.length > 0}
      <button class="clear" onclick={clearQueue}>Clear</button>
    {/if}
  </header>

  {#if player.upNext.length === 0}
    <p class="empty">
      {player.contextLen === 0
        ? "Open a folder to start listening."
        : "Playing through the library — nothing queued."}
    </p>
  {:else}
    <ol class="list">
      {#each player.upNext as id (id)}
        <li>
          <span class="dot" aria-hidden="true"></span>
          <span class="t">{titleFor(id)}</span>
        </li>
      {/each}
    </ol>
  {/if}

  <footer class="q-foot">
    <span>Context {player.contextPos + 1} / {player.contextLen}</span>
  </footer>
</section>

<style>
  .queue {
    display: flex;
    flex-direction: column;
    min-height: 0;
    height: 100%;
  }
  .q-header {
    display: flex;
    align-items: center;
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
  .clear {
    background: none;
    border: none;
    color: var(--fg-dim);
    font-size: 0.75rem;
    cursor: pointer;
    padding: 0.1rem 0.3rem;
  }
  .clear:hover {
    color: var(--accent);
  }
  .empty {
    margin: 1.5rem 0.75rem;
    color: var(--fg-dim);
    font-size: 0.8rem;
    text-align: center;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0.25rem 0;
    overflow-y: auto;
    flex: 1;
  }
  .list li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.25rem 0.75rem;
    font-size: 0.85rem;
  }
  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
    flex: none;
  }
  .t {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .q-foot {
    border-top: 1px solid color-mix(in srgb, var(--fg) 12%, transparent);
    padding: 0.35rem 0.75rem;
    font-size: 0.75rem;
    color: var(--fg-dim);
  }
</style>
