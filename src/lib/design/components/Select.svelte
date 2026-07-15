<script lang="ts" module>
  import type { Snippet } from "svelte";

  /** A single choice in a `Select` list. */
  export interface SelectItem {
    value: string;
    label: string;
    /** Optional leading icon, rendered as a snippet. */
    icon?: Snippet;
  }
</script>

<script lang="ts">
  // Lightweight single-value dropdown — a trigger button showing the current
  // selection, opening a plain popover list below it. Lighter than ModelPicker:
  // no search box, no groups. Mirrors `.select` / `.select-trigger` / `.select-pop`.
  interface Props {
    /** Every selectable choice, in display order. */
    items: SelectItem[];
    /** The currently selected item's `value`. */
    value: string;
    onchange?: (value: string) => void;
    /** Shown on the trigger when `value` matches no item. */
    placeholder?: string;
    disabled?: boolean;
  }

  let {
    items,
    value,
    onchange,
    placeholder = "Select…",
    disabled,
  }: Props = $props();

  let open = $state(false);
  let root: HTMLDivElement;

  let selected = $derived(items.find((i) => i.value === value));

  $effect(() => {
    if (!open) return;
    const onDocMouseDown = (e: MouseEvent) => {
      if (root && !root.contains(e.target as Node)) open = false;
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") open = false;
    };
    document.addEventListener("mousedown", onDocMouseDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("mousedown", onDocMouseDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  });
</script>

<div bind:this={root} class="relative inline-flex">
  <button
    type="button"
    class="inline-flex w-full items-center gap-[7px] rounded-[var(--r)] border border-border bg-surface px-[9px] py-[6px] text-[12.5px] font-medium text-text transition
      hover:enabled:border-border-strong hover:enabled:bg-surface-hover
      disabled:cursor-not-allowed disabled:opacity-50"
    aria-haspopup="listbox"
    aria-expanded={open}
    {disabled}
    onclick={() => (open = !open)}
  >
    {#if selected?.icon}
      <span class="grid shrink-0 place-items-center text-text-3">
        {@render selected.icon()}
      </span>
    {/if}
    <span
      class="min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap text-left
        {selected ? '' : 'text-text-3'}"
    >
      {selected ? selected.label : placeholder}
    </span>
    <svg
      class="shrink-0 text-text-3 transition-transform duration-[.12s] {open
        ? 'rotate-180'
        : ''}"
      width="11"
      height="11"
      viewBox="0 0 12 12"
      fill="none"
      stroke="currentColor"
      stroke-width="1.6"
    >
      <path d="M2 4.5 6 8l4-3.5" />
    </svg>
  </button>

  <div
    class="absolute left-0 top-[calc(100%+6px)] z-40 w-max min-w-full max-w-[280px] overflow-hidden rounded-[var(--r)] border border-border-strong bg-surface shadow-[var(--shadow-pop)] transition duration-[.12s]
      {open
      ? 'visible translate-y-0 opacity-100'
      : 'invisible translate-y-[4px] opacity-0 pointer-events-none'}"
  >
    <div class="max-h-[250px] overflow-y-auto p-[5px]" role="listbox">
      {#each items as item (item.value)}
        <button
          type="button"
          role="option"
          aria-selected={item.value === value}
          class="flex w-full items-center gap-[9px] rounded-[var(--r-sm)] border-0 bg-transparent px-[9px] py-[7px] text-left text-[12.5px] text-text transition hover:bg-surface-hover
            {item.value === value ? 'bg-accent-soft' : ''}"
          onclick={() => {
            onchange?.(item.value);
            open = false;
          }}
        >
          {#if item.icon}
            <span class="grid shrink-0 place-items-center text-text-3">
              {@render item.icon()}
            </span>
          {/if}
          <span
            class="min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap"
          >
            {item.label}
          </span>
          <svg
            class="shrink-0 text-accent {item.value === value
              ? 'opacity-100'
              : 'opacity-0'}"
            width="13"
            height="13"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2.5"
          >
            <path d="M5 12l4 4L19 6" />
          </svg>
        </button>
      {/each}
    </div>
  </div>
</div>
