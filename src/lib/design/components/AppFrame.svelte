<script lang="ts">
  // The app's root surface. Applies the design system's background + text color
  // so anything inside renders on-brand (the system is dark-first). Wrap a screen
  // in it. Maps to the app's root shell. Mirrors React `AppFrame`.
  import type { Snippet } from "svelte";

  interface Props {
    /** Force a theme; defaults to the design system's dark theme. */
    theme?: "dark" | "light";
    children: Snippet;
  }

  let { theme = "dark", children }: Props = $props();
</script>

<div class="min-h-full bg-bg p-6 text-text" data-theme={theme}>
  {@render children()}
</div>
