<script lang="ts">
  // Generic segmented control used in Settings (Memory privacy, Skills autonomy…).
  // `.seg` wrapper + inner buttons; the selected one gets the raised `.on` look.
  interface Option {
    value: string;
    label: string;
  }
  interface Props {
    options: Option[];
    value: string;
    onchange?: (value: string) => void;
  }

  let { options, value, onchange }: Props = $props();
</script>

<div
  role="radiogroup"
  class="inline-flex shrink-0 gap-0.5 rounded-[var(--r)] border border-border bg-surface-2 p-0.5"
>
  {#each options as o (o.value)}
    <button
      type="button"
      role="radio"
      aria-checked={value === o.value}
      onclick={() => onchange?.(o.value)}
      class="rounded-[var(--r-sm)] px-[11px] py-1 text-[11.5px] font-medium transition
        {value === o.value
        ? 'bg-surface text-text shadow-[var(--shadow)]'
        : 'bg-transparent text-text-3 hover:text-text-2'}"
    >
      {o.label}
    </button>
  {/each}
</div>
