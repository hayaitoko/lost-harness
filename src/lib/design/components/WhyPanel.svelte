<script lang="ts" module>
  import type { Snippet } from "svelte";
  import type { Route } from "../types";

  export interface WhyPanelSpan {
    /** Category label, e.g. "home address". */
    cat: string;
    /** Which layer caught it — a static rule match or the model's own read. */
    layer: "rule" | "model";
    /** Hard-blocked categories (health info, credentials, …) can never leave. */
    hard?: boolean;
  }
</script>

<script lang="ts">
  // The right-hand explainability panel — why a message was routed the way it
  // was, the annotated message with its detected spans, and (when relevant)
  // exactly what was redacted before leaving. Maps to `.why`.
  //
  // `message` / `redaction.kept` / `redaction.sent` are caller-authored markup
  // (React ReactNode → Snippet). The annotated message wraps detected text in
  // <mark class="span"> / <mark class="span hard"> — styled via scoped :global.

  interface Redaction {
    kept: Snippet;
    sent: Snippet;
  }

  interface Props {
    /** The routing decision this message actually took. */
    verdict: Route;
    /** Model that answered, e.g. "Qwen3-14B" or "Claude Opus 4.8". */
    model: string;
    /** The user's message, pre-annotated with `.span` / `.span.hard` marks. */
    message: Snippet;
    /** Detected sensitive spans, for the legend under the annotated message. */
    spans?: WhyPanelSpan[];
    /** Present only when a message was redacted before leaving. */
    redaction?: Redaction;
    onclose?: () => void;
  }

  let { verdict, model, message, spans, redaction, onclose }: Props = $props();

  const VERDICT_COPY: Record<
    Route,
    { title: string; subtitle: (m: string) => string }
  > = {
    local: {
      title: "Kept on your machine",
      subtitle: (m) => "Answered by " + m,
    },
    cloud: {
      title: "Sent to the cloud",
      subtitle: (m) => "Answered by " + m,
    },
    blocked: {
      title: "Held from leaving",
      subtitle: (m) => "Answered locally by " + m + "; outbound step held",
    },
  };

  const verdictTone: Record<Route, string> = {
    local: "bg-local-soft",
    cloud: "bg-cloud-soft",
    blocked: "bg-blocked-soft",
  };
  const icoTone: Record<Route, string> = {
    local: "text-local",
    cloud: "text-cloud",
    blocked: "text-blocked",
  };

  let v = $derived(VERDICT_COPY[verdict]);
</script>

<!-- verdict icons, inlined (stroke = currentColor inherits the meaning color) -->
{#snippet lockIcon()}
  <svg
    width="15"
    height="15"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
  >
    <rect x="5" y="11" width="14" height="9" rx="2" />
    <path d="M8 11V8a4 4 0 0 1 8 0v3" />
  </svg>
{/snippet}
{#snippet cloudIcon()}
  <svg
    width="13"
    height="13"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
  >
    <path d="M6 18a4 4 0 0 1 .5-8 6 6 0 0 1 11.5 1.5A3.5 3.5 0 0 1 17.5 18H6Z" />
  </svg>
{/snippet}
{#snippet stopIcon()}
  <svg
    width="15"
    height="15"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
  >
    <circle cx="12" cy="12" r="9" />
    <path d="M5.6 5.6l12.8 12.8" />
  </svg>
{/snippet}

<aside
  class="flex min-w-0 flex-col overflow-hidden border-l border-border bg-sidebar"
>
  <div
    class="flex h-12 flex-shrink-0 items-center gap-2 border-b border-border pl-4 pr-[10px]"
  >
    <span class="text-[13px] font-semibold">Why this happened</span>
    <button
      type="button"
      title="Close"
      aria-label="Close panel"
      onclick={onclose}
      class="relative ml-auto grid h-[30px] w-[30px] cursor-pointer place-items-center rounded-[var(--r)] border border-transparent bg-transparent text-text-3 transition-[background,color] duration-100 hover:bg-surface-hover hover:text-text-2"
    >
      <svg
        width="15"
        height="15"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
      >
        <path d="M6 6l12 12M18 6 6 18" />
      </svg>
    </button>
  </div>

  <div class="overflow-y-auto p-4">
    <!-- verdict -->
    <div class="mb-5">
      <div
        class="flex items-center gap-[9px] rounded-[var(--r)] p-[11px] text-[12.5px] {verdictTone[
          verdict
        ]}"
      >
        <span class="flex-shrink-0 {icoTone[verdict]}">
          {#if verdict === "local"}{@render lockIcon()}
          {:else if verdict === "cloud"}{@render cloudIcon()}
          {:else}{@render stopIcon()}{/if}
        </span>
        <div>
          <div class="font-semibold text-text">{v.title}</div>
          <div class="text-[11px] text-text-2">{v.subtitle(model)}</div>
        </div>
      </div>
    </div>

    <!-- annotated message + legend -->
    <div class="mb-5">
      <h4
        class="mb-[9px] text-[10.5px] font-[650] uppercase tracking-[0.05em] text-text-3"
      >
        Your message
      </h4>
      <div
        class="lh-annotated rounded-[var(--r)] border border-border bg-surface px-[13px] py-[11px] text-[13px] leading-[1.7]"
      >
        {@render message()}
      </div>
      {#if spans && spans.length > 0}
        <div class="mt-[11px] flex flex-col gap-[7px]">
          {#each spans as s, i (i)}
            <div class="flex items-center gap-[9px] text-[11.5px]">
              <span
                class="h-[11px] w-[11px] flex-shrink-0 rounded-[3px] border {s.hard
                  ? 'border-blocked bg-blocked-soft'
                  : 'border-warn bg-warn-soft'}"
              ></span>
              <span class="font-[550] text-text">
                {s.cat}{#if s.hard}<span class="font-semibold text-blocked">
                    · hard-block</span
                  >{/if}
              </span>
              <span
                class="ml-auto rounded-full bg-surface-2 px-[6px] py-[1.5px] text-[10px] font-semibold text-text-3"
              >
                {s.layer === "rule" ? "rule" : "model"}
              </span>
            </div>
          {/each}
        </div>
      {:else}
        <div class="text-[12px] text-text-3">
          No sensitive spans detected — nothing needed to stay local.
        </div>
      {/if}
    </div>

    <!-- redaction -->
    {#if redaction}
      <div class="mb-5 last:mb-0">
        <h4
          class="mb-[9px] text-[10.5px] font-[650] uppercase tracking-[0.05em] text-text-3"
        >
          Redaction — what actually left
        </h4>
        <div class="grid grid-cols-1 gap-[9px]">
          <div class="rounded-[var(--r)] border border-border px-3 py-[10px]">
            <div
              class="mb-[6px] flex items-center gap-[6px] text-[10.5px] font-[650] uppercase tracking-[0.04em] text-local"
            >
              {@render lockIcon()} Kept local
            </div>
            <div class="text-[12.5px] leading-[1.6] text-text-2">
              {@render redaction.kept()}
            </div>
          </div>
          <div class="rounded-[var(--r)] border border-border px-3 py-[10px]">
            <div
              class="mb-[6px] flex items-center gap-[6px] text-[10.5px] font-[650] uppercase tracking-[0.04em] text-cloud"
            >
              {@render cloudIcon()} Sent to cloud
            </div>
            <div class="text-[12.5px] leading-[1.6] text-text-2">
              {@render redaction.sent()}
            </div>
          </div>
        </div>
      </div>
    {/if}
  </div>
</aside>

<style>
  /* Irreducible: the annotated message is caller-authored markup that wraps
     detected text in <mark class="span"> / <mark class="span hard">. Mirrors
     the design's `.span` / `.span.hard`. */
  .lh-annotated :global(.span) {
    border-radius: 3px;
    padding: 0 2px;
    background: var(--warn-soft);
    border-bottom: 1.5px dashed var(--warn);
    cursor: help;
  }
  .lh-annotated :global(.span.hard) {
    background: var(--blocked-soft);
    border-bottom-color: var(--blocked);
  }
</style>
