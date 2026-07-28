import { defineConfig } from "vitest/config";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { resolve } from "node:path";

export default defineConfig({
  plugins: [svelte({ configFile: "svelte.config.js" })],
  resolve: {
    alias: {
      $lib: resolve(__dirname, "src/lib"),
    },
    conditions: ["browser"],
  },
  test: {
    environment: "jsdom",
    globals: true,
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    setupFiles: [resolve(__dirname, "src/tests/setup.ts")],
    // Vitest should not try to process Tauri's native modules.
    deps: {
      inline: [/@tauri-apps/],
    },
  },
});