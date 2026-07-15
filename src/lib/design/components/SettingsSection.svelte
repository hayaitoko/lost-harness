<script lang="ts">
  // A titled group of setting rows in the Settings panel. Maps to `.set-sec`.
  import type { Snippet } from "svelte";

  interface Props {
    /** Section heading, e.g. "Hard-block categories". */
    title: string;
    children: Snippet;
  }

  let { title, children }: Props = $props();
</script>

<div class="mb-5 last:mb-0">
  <h3
    class="mb-2 text-[11px] font-[650] uppercase tracking-[0.05em] text-text-3"
  >
    {title}
  </h3>
  {@render children()}
</div>
