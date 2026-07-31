<script lang="ts">
  // The non-silent censorship alert that appears inline in the thread when the
  // egress guard keeps something local or holds an outbound step. Maps to `.event`.
  // `kept` = green "stayed on your machine"; `stop` = red "held from leaving";
  // `unknown` = amber "we can't confirm where this went".
  //
  // `unknown` exists because `kept` is a PRIVACY CLAIM, and the app is only
  // entitled to make it when the backend stamped the turn's trust zone as local
  // (`served_by.zone`). A turn recorded before that stamp existed still carries
  // its routing DECISION, which says the gate rerouted it — a different, weaker
  // fact. Painting that green would be the same reassuring lie the route badge
  // was fixed to stop telling, so it gets the badge's own amber "anomaly worth
  // noticing" tone instead.
  import type { Snippet } from "svelte";

  interface Props {
    kind: "kept" | "stop" | "unknown";
    title: string;
    children?: Snippet;
    /** Inline action links (e.g. "What tripped it", "Approve the send"). */
    links?: Snippet;
  }

  let { kind, title, children, links }: Props = $props();

  const tone = {
    kept: "bg-local-soft border-l-local text-local",
    stop: "bg-blocked-soft border-l-blocked text-blocked",
    // Matches `RoutingBadge`'s unknown tone, so the bar and the badge on the
    // same turn read as one statement rather than two moods.
    unknown: "bg-warn-soft border-l-warn text-warn",
  };
</script>

<div
  class="mx-auto flex w-full max-w-[700px] items-start gap-[11px] rounded-r-[var(--r)] border-l-2 px-[13px] py-[10px] text-[12.5px] {tone[
    kind
  ]}"
>
  <div class="mt-px flex-shrink-0">
    {#if kind === "kept"}
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
    {:else if kind === "unknown"}
      <!-- An OPEN padlock: deliberately the same lock silhouette as `kept`,
           unlatched. "We can't confirm this stayed put" should read as a
           near-miss of the reassuring icon, not as a different subject. -->
      <svg
        width="15"
        height="15"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
      >
        <rect x="5" y="11" width="14" height="9" rx="2" />
        <path d="M8 11V8a4 4 0 0 1 8 0" />
      </svg>
    {:else}
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
    {/if}
  </div>
  <div>
    <div class="mb-0.5 font-semibold text-text">{title}</div>
    {#if children}
      <div class="lh-ev-body leading-[1.5] text-text-2">
        {@render children()}
      </div>
    {/if}
    {#if links}
      <div class="mt-[5px] flex flex-wrap gap-[14px]">
        {@render links()}
      </div>
    {/if}
  </div>
</div>

<style>
  /* Irreducible: `.ev-body b` promotes bold spans in caller-authored body copy
     to the full-strength text color (design's `.event .ev-body b`). */
  .lh-ev-body :global(b) {
    color: var(--text);
    font-weight: 600;
  }
</style>
