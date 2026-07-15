<script lang="ts">
  // A single row of the Settings panel: optional status dot, a title/description
  // pair, an optional status tag, and a right-side control. Maps to `.set-row`.
  // The `control` slot takes a Toggle/SegmentedControl/select/button/value text.
  import type { Snippet } from "svelte";

  interface Tag {
    label: string;
    /** CSS color for the tag text (routing signal — passed by caller). */
    color?: string;
    /** CSS background for the tag (routing signal — passed by caller). */
    bg?: string;
  }
  interface Props {
    title: string;
    /** Secondary explanatory line under the title. */
    desc?: string;
    /** CSS color for a small status dot to the left of the row. */
    dotColor?: string;
    /** Small pill shown after the row's text (e.g. "online", "free"). */
    tag?: Tag;
    /** Right-side control — toggle, segmented control, select, button, value text. */
    control?: Snippet;
  }

  let { title, desc, dotColor, tag, control }: Props = $props();
</script>

<div
  class="mb-1.5 flex items-center gap-[11px] rounded-[var(--r)] border border-border bg-surface px-3 py-2.5"
>
  {#if dotColor}
    <span
      class="h-[7px] w-[7px] shrink-0 rounded-full"
      style="background:{dotColor}"
    ></span>
  {/if}
  <div class="min-w-0 flex-1">
    <div class="text-[13px] font-[550]">{title}</div>
    {#if desc}
      <div class="text-[11.5px] leading-[1.4] text-text-3">{desc}</div>
    {/if}
  </div>
  {#if tag}
    <span
      class="shrink-0 rounded-[var(--r-sm)] px-[7px] py-0.5 text-[10px] font-semibold"
      style="background:{tag.bg ?? ''};color:{tag.color ?? ''}"
    >
      {tag.label}
    </span>
  {/if}
  {@render control?.()}
</div>
