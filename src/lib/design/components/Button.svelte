<script lang="ts">
  // Standard action button — the design system's `.btn` family, translated to
  // Tailwind utilities. `primary` is the accent action; `ghost` is borderless.
  import type { Snippet } from "svelte";

  interface Props {
    variant?: "default" | "primary" | "ghost";
    type?: "button" | "submit";
    disabled?: boolean;
    onclick?: () => void;
    children: Snippet;
  }

  let {
    variant = "default",
    type = "button",
    disabled = false,
    onclick,
    children,
  }: Props = $props();

  const base =
    "inline-flex items-center justify-center gap-1.5 rounded-[var(--r)] border px-[13px] py-[7px] text-[12.5px] font-semibold transition disabled:cursor-not-allowed disabled:opacity-60";
  const variants = {
    default: "border-border-strong bg-surface text-text hover:bg-surface-hover",
    primary: "border-transparent bg-accent text-on-accent hover:brightness-105",
    ghost:
      "border-transparent bg-transparent text-text-2 hover:bg-surface-hover hover:text-text",
  };
</script>

<button {type} {disabled} {onclick} class="{base} {variants[variant]}">
  {@render children()}
</button>
