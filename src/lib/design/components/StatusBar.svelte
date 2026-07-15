<script lang="ts">
  // Bottom status bar: engine/host, guard state, tool/skill counts on the left;
  // binding, last-turn disposition, session, cost and version on the right.
  // Maps to `<footer class="statusbar">`.
  import type { Route, Binding } from "../types";

  interface Props {
    /** Active model label, e.g. "Qwen3-14B". */
    engine?: string;
    /** Machine the engine is running on, e.g. "tadashi". */
    host?: string;
    /** Whether the egress guard (classifier) is active. */
    guardOn?: boolean;
    /** Number of tools available to the model. */
    tools?: number;
    /** Number of skills available to the model. */
    skills?: number;
    /** Current conversation binding (Auto / Public / Private). */
    binding?: Binding;
    /** Disposition of the most recent turn; null/undefined renders "—". */
    lastRoute?: Route | null;
    /** Session duration, e.g. "0:04". */
    session?: string;
    /** Running session cost, e.g. "$0.00". */
    cost?: string;
    /** Build/version string, e.g. "v0.1 · mockup". */
    version?: string;
  }

  let {
    engine = "Qwen3-14B",
    host = "tadashi",
    guardOn = true,
    tools = 3,
    skills = 2,
    binding = "auto",
    lastRoute = null,
    session = "0:04",
    cost = "$0.00",
    version = "v0.1 · mockup",
  }: Props = $props();

  const ROUTE_LABEL: Record<Route, string> = {
    local: "Local",
    cloud: "Cloud",
    blocked: "Held",
  };
  // Route dot fill — grayscale default, meaning-color per disposition.
  const ROUTE_DOT: Record<Route, string> = {
    local: "bg-local",
    cloud: "bg-cloud",
    blocked: "bg-blocked",
  };

  const st = "inline-flex items-center gap-[5px] whitespace-nowrap";

  let bindingLabel = $derived(binding.charAt(0).toUpperCase() + binding.slice(1));
</script>

<footer
  class="flex items-center gap-[14px] h-[26px] flex-shrink-0 px-3 border-t border-border bg-sidebar text-[11px] text-text-3"
>
  <span class={st}>
    <span class="h-[6px] w-[6px] rounded-full bg-local"></span>
    <b class="font-semibold text-text-2">{engine}</b> · {host}
  </span>
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
  <span class="{st} cursor-pointer hover:text-text-2">Tools {tools}</span>
  <span class="{st} cursor-pointer hover:text-text-2">Skills {skills}</span>
  <span class="{st} ml-auto"></span>
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
  <span class={st}>
    <span
      class="h-[6px] w-[6px] rounded-full {lastRoute ? ROUTE_DOT[lastRoute] : 'bg-text-3'}"
    ></span>
    Last: <b class="font-semibold text-text-2">{lastRoute ? ROUTE_LABEL[lastRoute] : "—"}</b>
  </span>
  <span class={st}>Session {session}</span>
  <span class={st}>
    <b class="font-semibold text-text-2">{cost}</b> · local
  </span>
  <span class={st}>{version}</span>
</footer>
