<script lang="ts">
  // Switch toggle used across Settings — `.tgl`. The sliding knob is the `::after`
  // pseudo-element, expressed with Tailwind `after:` utilities. Locked toggles
  // (e.g. the hard-block categories) can't be changed.
  interface Props {
    checked: boolean;
    locked?: boolean;
    label?: string;
    onchange?: (value: boolean) => void;
  }

  let { checked, locked = false, label, onchange }: Props = $props();
</script>

<button
  type="button"
  role="switch"
  aria-checked={checked}
  aria-label={label}
  aria-disabled={locked || undefined}
  disabled={locked}
  onclick={() => {
    if (!locked) onchange?.(!checked);
  }}
  class="relative h-5 w-[34px] shrink-0 cursor-pointer rounded-full border transition
    after:absolute after:left-0.5 after:top-0.5 after:h-3.5 after:w-3.5 after:rounded-full after:transition
    {checked
    ? 'border-transparent bg-accent after:left-4 after:bg-white'
    : 'border-border-strong bg-surface-2 after:bg-text-3'}
    {locked ? 'cursor-not-allowed opacity-60' : ''}"
  aria-hidden={label ? undefined : true}
></button>
