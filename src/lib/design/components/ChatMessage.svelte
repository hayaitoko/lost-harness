<script lang="ts">
  // A single turn in the thread. `user` → right-aligned bubble; `assistant` →
  // shield mark + name + optional routing badge, then the response body.
  // `.msg.user` / `.msg.assistant`. The badge is a Snippet prop so callers can
  // drop a <RoutingBadge/> next to the name.
  import type { Snippet } from "svelte";

  interface Props {
    role: "user" | "assistant";
    name?: string;
    /** Routing-badge slot next to the assistant name. */
    badge?: Snippet;
    children: Snippet;
  }

  let { role, name = "Lost Harness", badge, children }: Props = $props();
</script>

{#if role === "user"}
  <div class="flex justify-end">
    <div
      class="max-w-[82%] rounded-[var(--r-lg)] border border-border bg-surface-2 px-[13px] py-[9px] text-[14px] leading-[1.55]"
    >
      {@render children()}
    </div>
  </div>
{:else}
  <div>
    <div class="mb-[7px] flex items-center gap-2">
      <span
        class="grid h-5 w-5 place-items-center rounded-[var(--r-sm)] border border-border-strong bg-surface-2 text-text-2"
      >
        <svg
          width="12"
          height="12"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        >
          <path d="M12 3 4 6v6c0 4 3 7 8 9 5-2 8-5 8-9V6l-8-3Z" />
        </svg>
      </span>
      <span class="text-[12.5px] font-semibold">{name}</span>
      {@render badge?.()}
    </div>
    <div class="lh-content pl-7 text-[14px] leading-[1.62]">
      {@render children()}
    </div>
  </div>
{/if}

<style>
  /* Irreducible descendant typography for assistant body content (the children
     are caller-authored markup, so it can't be styled with utilities). Mirrors
     the design's `.content p/code/pre`. */
  .lh-content :global(p) {
    margin: 0 0 9px;
  }
  .lh-content :global(p:last-child) {
    margin-bottom: 0;
  }
  .lh-content :global(code) {
    font-family: ui-monospace, "SF Mono", Menlo, monospace;
    font-size: 12.5px;
    background: var(--surface-2);
    padding: 1.5px 5px;
    border-radius: var(--r-sm);
  }
  .lh-content :global(pre) {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--r);
    padding: 11px 13px;
    overflow-x: auto;
    margin: 6px 0 0;
    font-family: ui-monospace, "SF Mono", Menlo, monospace;
    font-size: 12.5px;
    line-height: 1.55;
  }
</style>
