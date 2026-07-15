<script lang="ts">
  // Per-message routing chip — `.badge` filled by meaning-color. Green = local,
  // blue = cloud, red = held. Renders a real <button> when `onclick` is set
  // (opens the Why panel), else a static <span>.
  import type { Route } from "../types";

  interface Props {
    route: Route;
    /** Override the label (defaults: Local / Cloud / Held). */
    label?: string;
    /** When set, the badge is an interactive button. */
    onclick?: () => void;
  }

  let { route, label, onclick }: Props = $props();

  const DEFAULTS: Record<Route, string> = {
    local: "Local",
    cloud: "Cloud",
    blocked: "Held",
  };
  const tone = {
    local: "bg-local-soft text-local",
    cloud: "bg-cloud-soft text-cloud",
    blocked: "bg-blocked-soft text-blocked",
  };
  const base =
    "inline-flex items-center gap-[5px] rounded-[var(--r-sm)] px-[7px] py-0.5 text-[10.5px] font-medium";
</script>

{#snippet inner()}
  <span class="h-[5px] w-[5px] rounded-full bg-current"></span>
  {label ?? DEFAULTS[route]}
{/snippet}

{#if onclick}
  <button type="button" {onclick} class="{base} {tone[route]} cursor-pointer hover:brightness-110">
    {@render inner()}
  </button>
{:else}
  <span class="{base} {tone[route]} cursor-default">
    {@render inner()}
  </span>
{/if}
