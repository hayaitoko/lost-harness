<script lang="ts">
  // Bottom status bar: engine/host, guard state, tool/skill counts on the left;
  // binding, last-turn disposition, session, cost and version on the right.
  // Maps to `<footer class="statusbar">`.
  //
  // Honest-Unknown: every segment renders ONLY when its prop is provided —
  // an undefined prop hides the segment entirely rather than showing a
  // made-up placeholder value.
  import type { Route, Binding } from "../types";

  interface Props {
    /** Active model label, e.g. "Qwen3-14B". Undefined hides the segment. */
    engine?: string;
    /** Machine the engine is running on, e.g. "tadashi". Undefined hides it. */
    host?: string;
    /** Whether the egress guard (classifier) is active. */
    guardOn?: boolean;
    /** Number of tools available to the model. Undefined hides the segment. */
    tools?: number;
    /** Number of skills available to the model. Undefined hides the segment. */
    skills?: number;
    /** Current conversation binding (Auto / Public / Private). Undefined hides it. */
    binding?: Binding;
    /** Disposition of the most recent turn; null renders "—" (no turn yet),
     * undefined hides the segment. */
    lastRoute?: Route | null;
    /** Session duration, e.g. "0:04". Undefined hides the segment. */
    session?: string;
    /** Running session cost, e.g. "$0.00". Undefined hides the segment. */
    cost?: string;
    /** Build/version string, e.g. "0.1.0-m1". Undefined hides the segment. */
    version?: string;
  }

  let {
    engine,
    host,
    // Default true is honest: the rules layer of the privacy classifier is
    // structurally always-on — lib.rs falls back to RulesClassifier when the
    // trained ensemble is absent, and PrivacyGate always wraps a classifier.
    guardOn = true,
    tools,
    skills,
    binding,
    lastRoute,
    session,
    cost,
    version,
  }: Props = $props();

  const ROUTE_LABEL: Record<Route, string> = {
    local: "Local",
    cloud: "Cloud",
    blocked: "Held",
    unknown: "Unknown",
  };
  // Route dot fill — grayscale default, meaning-color per disposition.
  const ROUTE_DOT: Record<Route, string> = {
    local: "bg-local",
    cloud: "bg-cloud",
    blocked: "bg-blocked",
    unknown: "bg-warn",
  };

  const st = "inline-flex items-center gap-[5px] whitespace-nowrap";

  let bindingLabel = $derived(
    binding !== undefined ? binding.charAt(0).toUpperCase() + binding.slice(1) : "",
  );
</script>

<footer
  class="flex items-center gap-[14px] h-[26px] flex-shrink-0 px-3 border-t border-border bg-sidebar text-[11px] text-text-3"
>
  {#if engine !== undefined || host !== undefined}
    <span class={st}>
      <span class="h-[6px] w-[6px] rounded-full bg-local"></span>
      {#if engine !== undefined}<b class="font-semibold text-text-2">{engine}</b>{/if}
      {#if engine !== undefined && host !== undefined}·{/if}
      {#if host !== undefined}{host}{/if}
    </span>
  {/if}
  <span class={st}>
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke={guardOn ? "var(--local)" : "var(--text-3)"}
      stroke-width="2"
      class="opacity-85"
    >
      <path d="M12 3 4 6v6c0 4 3 7 8 9 5-2 8-5 8-9V6l-8-3Z" />
    </svg>
    Guard {guardOn ? "on" : "off"}
  </span>
  {#if tools !== undefined}
    <span class="{st} cursor-pointer hover:text-text-2">Tools {tools}</span>
  {/if}
  {#if skills !== undefined}
    <span class="{st} cursor-pointer hover:text-text-2">Skills {skills}</span>
  {/if}
  <span class="{st} ml-auto"></span>
  {#if binding !== undefined}
    <span class={st}>
      <svg
        width="12"
        height="12"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        class="opacity-85"
      >
        <circle cx="12" cy="12" r="9" />
        <path d="M12 3v3M12 18v3M3 12h3M18 12h3" />
      </svg>
      Binding: <b class="font-semibold text-text-2">{bindingLabel}</b>
    </span>
  {/if}
  {#if lastRoute !== undefined}
    <span class={st}>
      <span
        class="h-[6px] w-[6px] rounded-full {lastRoute ? ROUTE_DOT[lastRoute] : 'bg-text-3'}"
      ></span>
      Last: <b class="font-semibold text-text-2">{lastRoute ? ROUTE_LABEL[lastRoute] : "—"}</b>
    </span>
  {/if}
  {#if session !== undefined}
    <span class={st}>Session {session}</span>
  {/if}
  {#if cost !== undefined}
    <span class={st}>
      <b class="font-semibold text-text-2">{cost}</b> · local
    </span>
  {/if}
  {#if version !== undefined}
    <span class={st}>{version}</span>
  {/if}
</footer>
