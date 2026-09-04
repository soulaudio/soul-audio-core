import path from 'path';

const FIXTURE = path.resolve(process.cwd(), '../../testdata').replace(/\\/g, '/');

describe('soul-player smoke', () => {
  it('renders the shell', async () => {
    await expect($('body')).toBeExisting();
  });

  it('loads a folder and shows tracks + transport', async () => {
    await browser.execute((dir) => {
      // @ts-ignore — page context
      return window.__TAURI_INTERNALS__.invoke('load_folder_path', { dir });
    }, FIXTURE);

    // Freeze on Repeat::One. Under SOUL_PLAYER_OFFLINE, the sink is a
    // backpressure-free NullSink (soul-player/src/offline.rs) — the whole
    // fixture queue decodes and "plays" in a fraction of a millisecond. With
    // the default Repeat::Off, `TwoLevelQueue::advance` clears `cur` to
    // `None` the instant a non-repeating source runs out
    // (soul-audio-queue/src/queue.rs), so by the time WebDriver's next poll
    // lands, now-playing has already reverted to nothing — unobservable, not
    // a real bug. Repeat::One freezes `cur` on natural end instead of
    // clearing it, giving the assertions below a stable target.
    await browser.execute(() => {
      // @ts-ignore — page context
      return window.__TAURI_INTERNALS__.invoke('send_command', { cmd: { SetRepeat: 'One' } });
    });

    // `load_folder_path` (like `open_folder`) only returns the scanned views
    // to its caller — it doesn't push them into the Svelte store, and the
    // "player" event channel deliberately never carries the library (see
    // store.svelte.ts). The app's own hydration path (`bindPlayerEvents`,
    // run from `App.svelte`'s `onMount`) re-fetches `library_views` on
    // mount ("fast path after re-open"). A webview reload re-runs that
    // mount against the same in-process backend state (the scan we just
    // injected), which is the same path a real re-open of the app takes.
    await browser.refresh();

    // Library rows appear (adapt the selector to Library.svelte's markup).
    await browser.waitUntil(async () => (await $$('[data-testid="track-row"]')).length > 0, {
      timeout: 10000,
      timeoutMsg: 'no track rows rendered after load_folder_path',
    });

    // Now-playing title shows in the player bar (adapt selector).
    await expect($('[data-testid="now-playing-title"]')).toBeExisting();
  });

  it('clicking a row changes now-playing', async () => {
    const rows = await $$('[data-testid="track-row"]');
    const title = await $('[data-testid="now-playing-title"]');
    const before = await title.getText();
    await rows[rows.length - 1].click();
    await browser.waitUntil(async () => (await title.getText()) !== before, { timeout: 5000 });
  });
});
