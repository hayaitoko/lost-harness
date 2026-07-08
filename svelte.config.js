import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

export default {
  // Preprocess Svelte components with vitePreprocess for TS/PostCSS
  preprocess: vitePreprocess(),

  // Svelte 5: runes are the default, but set explicitly for clarity.
  // Any component using $state/$derived/$effect/etc. works automatically.
  compilerOptions: {
    runes: true,
  },
};
