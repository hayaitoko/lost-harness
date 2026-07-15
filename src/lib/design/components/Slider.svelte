<script lang="ts">
  // Labeled range slider primitive. Grayscale track (--surface-2) with an
  // --accent thumb — an active control, not a routing signal. The range
  // track/thumb pseudo-elements live in the global .lh-range rules in app.css
  // (svelte2tsx mis-parses vendor pseudo-elements in a scoped <style>).
  interface Props {
    label?: string;
    min: number;
    max: number;
    step?: number;
    value: number;
    onchange?: (value: number) => void;
    /** Pre-formatted value text (e.g. "$30"). Wins over format if both are given. */
    valueLabel?: string;
    /** Formats the numeric value into display text. Ignored if valueLabel is set. */
    format?: (value: number) => string;
    disabled?: boolean;
  }

  let {
    label,
    min,
    max,
    step = 1,
    value,
    onchange,
    valueLabel,
    format,
    disabled = false,
  }: Props = $props();

  let displayValue = $derived(valueLabel ?? (format ? format(value) : String(value)));
</script>

<div class="flex flex-col gap-1.5 {disabled ? 'opacity-50' : ''}">
  {#if label}
    <div class="flex items-center justify-between gap-[11px]">
      <span class="text-[13px] font-[550] text-text">{label}</span>
      <span
        class="shrink-0 whitespace-nowrap rounded-[var(--r-sm)] bg-surface-2 px-[7px] py-0.5 text-[10px] font-semibold text-text-2"
      >
        {displayValue}
      </span>
    </div>
  {/if}
  <div class="flex flex-1 items-center gap-[11px]">
    <input
      class="lh-range"
      type="range"
      {min}
      {max}
      {step}
      {value}
      {disabled}
      aria-label={label ?? "Slider"}
      aria-valuetext={displayValue}
      oninput={(e) => onchange?.(Number(e.currentTarget.value))}
    />
    {#if !label}
      <span
        class="shrink-0 whitespace-nowrap rounded-[var(--r-sm)] bg-surface-2 px-[7px] py-0.5 text-[10px] font-semibold text-text-2"
      >
        {displayValue}
      </span>
    {/if}
  </div>
</div>
