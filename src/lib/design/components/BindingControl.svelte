<script lang="ts">
  // The top-bar Auto / Public / Private control — the conversation's binding.
  // Private tints green (stays local), Public tints blue (cloud-OK), Auto is
  // neutral/raised. Maps to `.binding`.
  import type { Binding } from "../types";

  interface Props {
    /** Current binding (the user's routing intent). */
    value: Binding;
    onchange?: (b: Binding) => void;
  }

  let { value, onchange }: Props = $props();

  const OPTS: { id: Binding; label: string }[] = [
    { id: "auto", label: "Auto" },
    { id: "public", label: "Public" },
    { id: "private", label: "Private" },
  ];

  // `.on` look per binding: Auto raises grayscale, Public tints cloud, Private tints local.
  const onTone: Record<Binding, string> = {
    auto: "bg-surface text-text shadow-[var(--shadow)]",
    public: "bg-cloud-soft text-cloud",
    private: "bg-local-soft text-local",
  };
</script>

<div
  role="radiogroup"
  aria-label="Conversation binding"
  class="inline-flex shrink-0 gap-0.5 rounded-[var(--r)] border border-border bg-surface-2 p-0.5"
>
  {#each OPTS as o (o.id)}
    <button
      type="button"
      role="radio"
      aria-checked={value === o.id}
      onclick={() => onchange?.(o.id)}
      class="inline-flex items-center gap-[6px] rounded-[var(--r-sm)] px-[10px] py-1 text-[12px] font-[550] transition
        {value === o.id ? onTone[o.id] : 'bg-transparent text-text-3 hover:text-text-2'}"
    >
      {#if o.id === "auto"}
        <span class="h-[6px] w-[6px] rounded-full bg-current"></span>
      {:else if o.id === "public"}
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M6 18a4 4 0 0 1 .5-8 6 6 0 0 1 11.5 1.5A3.5 3.5 0 0 1 17.5 18H6Z" />
        </svg>
      {:else}
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <rect x="5" y="11" width="14" height="9" rx="2" />
          <path d="M8 11V8a4 4 0 0 1 8 0v3" />
        </svg>
      {/if}
      {o.label}
    </button>
  {/each}
</div>
