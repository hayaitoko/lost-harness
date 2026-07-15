<script lang="ts">
  // Multi-device baton notice — another device is actively driving this
  // conversation; offers to view read-only or ask for the baton back. Maps to
  // the design's `.baton` (warn-tinted strip). Rendered only when relevant, so
  // this is always the "show" state.
  interface Props {
    /** The other device currently holding the conversation, e.g. "desktop". */
    device: string;
    /** "Open read-only" — view the conversation without taking control. */
    onopenreadonly?: () => void;
    /** "Ask it to hand over" — request the other device release control. */
    onhandover?: () => void;
    /** Dismiss the banner (the close X). */
    ondismiss?: () => void;
  }

  let { device, onopenreadonly, onhandover, ondismiss }: Props = $props();

  const btn =
    "rounded-[var(--r-sm)] border border-border-strong bg-surface px-[10px] py-[4px] text-[11.5px] font-[550]";
</script>

<div
  class="flex flex-shrink-0 items-center gap-[10px] border-b border-[color-mix(in_srgb,var(--warn)_26%,transparent)] bg-warn-soft px-[18px] py-[7px] text-[12.5px] text-text"
>
  <span class="grid place-items-center text-warn">
    <svg
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
    >
      <path d="M12 8v5l3 2" />
      <circle cx="12" cy="12" r="9" />
    </svg>
  </span>
  <span>
    Your <b>{device}</b> is working on this conversation.
  </span>
  <div class="ml-auto flex gap-[6px]">
    <button type="button" class={btn} onclick={onopenreadonly}>
      Open read-only
    </button>
    <button type="button" class={btn} onclick={onhandover}>
      Ask it to hand over
    </button>
  </div>
  <button
    type="button"
    aria-label="Dismiss"
    class="grid h-6 w-6 place-items-center rounded-[var(--r-sm)] border-0 bg-transparent text-text-3"
    onclick={ondismiss}
  >
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
    >
      <path d="M6 6l12 12M18 6 6 18" />
    </svg>
  </button>
</div>
