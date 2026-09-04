import { expect, test } from 'vitest';
import { render } from 'vitest-browser-svelte';
import Smoke from './Smoke.svelte';

test('renders and reacts to a rune click', async () => {
  const screen = await render(Smoke, { label: 'hi' });
  const btn = screen.getByRole('button');
  await expect.element(btn).toHaveTextContent('hi: 0');
  await btn.click();
  await expect.element(btn).toHaveTextContent('hi: 1');
});
