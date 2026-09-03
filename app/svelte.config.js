import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

export default {
  preprocess: vitePreprocess(),
  compilerOptions: {
    // Runes mode everywhere: $state/$derived/$props are compiler-native.
    runes: true,
  },
};
