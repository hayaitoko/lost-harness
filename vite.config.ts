import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";
import { resolve } from "node:path";

// Tauri expects a fixed port and to fail if it's not available
const host = process.env.TAURI_DEV_HOST;

// Vite resolves `$lib` to `src/lib` for us (matches the SvelteKit
// convention even though this isn't a SvelteKit project). Components and
// stores import via `$lib/components/...` and `$lib/stores/...`.
const LIB_ALIAS = { $lib: resolve(__dirname, "src/lib") };

export default defineConfig(async () => ({
  // `root` is `src/`, while the one canonical Svelte config lives at the
  // repository root. Point the plugin at it explicitly so both Vite and
  // svelte-check share the same config without a duplicate source file.
  plugins: [tailwindcss(), svelte({ configFile: "../svelte.config.js" })],

  // Prevent Vite from obscuring Rust errors
  clearScreen: false,

  // Vite's "root" is the directory containing index.html. We point it at
  // src/ so app.html is the entry, then explicitly configure the rollup
  // input to match the file name. Svelte/Tauri apps can use either pattern;
  // this lets us keep app.html under src/ as the spec requires.
  root: "src",
  resolve: {
    alias: LIB_ALIAS,
  },
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    rollupOptions: {
      input: resolve(__dirname, "src/app.html"),
    },
    // Tauri uses WebView2 (Edge) on Windows and WebKit on macOS/Linux.
    // Tauri 2 ships with current WebKit (macOS 12+ ships WebKit equivalent
    // to safari15+). We target safari15 / chrome108 for Svelte 5's
    // destructuring patterns; esbuild can't transform nested destructuring
    // for older targets.
    target: process.env.TAURI_ENV_PLATFORM == "windows" ? "chrome108" : "safari15",
    // Don't minify for debug builds
    minify: !process.env.TAURI_ENV_DEBUG ? "esbuild" : false,
    // Produce sourcemaps for debug builds
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },

  // Tauri expects a fixed port; fail fast if it's not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    watch: {
      // Tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
  // Env variables starting with VITE_ are exposed to the client
  envPrefix: ["VITE_", "TAURI_ENV_*"],
}));
