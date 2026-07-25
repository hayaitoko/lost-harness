<script lang="ts" module>
  let knotSeq = 0;
</script>

<script lang="ts">
  // The Knot — Lost Harness's idle/status mark. A real trefoil (the simplest
  // knot that provably can't be untied without cutting the rope): one closed
  // line, no start, no end — an idle agent isn't *off*, it's *held*.
  //
  // One inline SVG + the scoped animation stylesheet (ported from knot.css) — no
  // GIF, no video; freezes to the still knot under prefers-reduced-motion. The
  // over/under weave is computed from the real 3-D curve (knot-geometry.ts); the
  // patrol light dips under the strand at each crossing via a mask cut from the
  // over-strands. Grayscale chrome, saturated color only for routing.
  //
  // Defs are scoped per instance so it's drop-in anywhere with no setup.
  import { FULL, OVER0, OVER1, OVER2 } from "./knot-geometry";

  /** Agent status conveyed by the idle mark. */
  export type KnotState = "idle" | "local" | "cloud" | "filter" | "held";

  interface Props {
    /**
     * What the agent is doing.
     * - `idle` — watching. All grayscale (breath + patrol + settle).
     * - `local` / `cloud` — actively answering; the patrol light takes the
     *   routing color (green / blue) and quickens.
     * - `held` — the egress guard stopped something; the light is pinned red at
     *   a crossing. Same meaning as a `blocked` route.
     */
    state?: KnotState;
    /** Rendered size in px (square). Default 64. */
    size?: number;
    /** Heavier rope for legibility at small sizes. Defaults on at size <= 28. */
    bold?: boolean;
    /** Animation-delay offset (seconds) to desync multiple marks on one screen. */
    seed?: number;
    /** Accessible label. Default "Agent status mark". */
    title?: string;
  }

  let {
    state = "idle",
    size = 64,
    bold,
    seed = 0,
    title = "Agent status mark",
  }: Props = $props();

  const uid = `k${knotSeq++}`;
  let isBold = $derived(bold ?? size <= 28);
  let maskW = $derived(isBold ? 13 : 8);

  const kId = `k-${uid}`;
  const o0 = `o0-${uid}`;
  const o1 = `o1-${uid}`;
  const o2 = `o2-${uid}`;
  const mId = `m-${uid}`;
  let mask = $derived(`url(#${mId})`);

  let style = $derived(
    [isBold ? "--sw:5px" : "", seed ? `--seed:${seed}s` : ""]
      .filter(Boolean)
      .join(";"),
  );
</script>

<svg
  class="knot"
  class:st-local={state === "local"}
  class:st-cloud={state === "cloud"}
  class:st-filter={state === "filter"}
  class:st-held={state === "held"}
  viewBox="0 0 100 100"
  width={size}
  height={size}
  role="img"
  aria-label={title}
  {style}
>
  <defs>
    <path id={kId} d={FULL} pathLength={100} />
    <path id={o0} d={OVER0} />
    <path id={o1} d={OVER1} />
    <path id={o2} d={OVER2} />
    <!-- Mask cut from the over-strands: knocks a gap in whatever passes under. -->
    <mask id={mId} maskUnits="userSpaceOnUse" x={-10} y={-10} width={120} height={120}>
      <rect x={-10} y={-10} width={120} height={120} fill="#fff" />
      <use href={`#${o0}`} fill="none" stroke="#000" stroke-width={maskW} stroke-linecap="round" />
      <use href={`#${o1}`} fill="none" stroke="#000" stroke-width={maskW} stroke-linecap="round" />
      <use href={`#${o2}`} fill="none" stroke="#000" stroke-width={maskW} stroke-linecap="round" />
    </mask>
  </defs>

  <g class="breathe">
    <g class="settle">
      <!-- Base line, masked so it dips under at each crossing. -->
      <use href={`#${kId}`} class="base" {mask} />
      <!-- Over-strands bridge the gaps — the honest over/under weave. -->
      <use href={`#${o0}`} class="over" />
      <use href={`#${o1}`} class="over" />
      <use href={`#${o2}`} class="over" />
      <!-- Patrol light: three layered dashes (glow + body + head) on the 11s clock… -->
      <use href={`#${kId}`} class="pulse p1" {mask} />
      <use href={`#${kId}`} class="pulse p2" {mask} />
      <use href={`#${kId}`} class="pulse p3" {mask} />
      <!-- …plus a second light walking the other way on the 26s clock. -->
      <use href={`#${kId}`} class="pulse pb" {mask} />
      <!-- Held: red glow pinned at one crossing (hidden unless st-held). -->
      <g class="heldg">
        <use href={`#${o1}`} class="hg" />
        <use href={`#${o1}`} class="hc" />
      </g>
    </g>
  </g>
</svg>

<style>
  /* ===== THE KNOT — ported verbatim from ui/src/styles/knot.css =====
   * Idle costs three dash offsets and one scale transform. Colors are the
   * routing tokens — grayscale until the agent crosses the boundary.
   * Tokens: --knot-line, --pulse-idle, --local, --cloud, --blocked. */
  .knot {
    display: block;
    overflow: visible;
  }
  .knot .breathe {
    animation: knot-breathe 7s ease-in-out infinite;
    transform-box: view-box;
    transform-origin: 50% 50%;
  }
  .knot .settle {
    animation: knot-settle var(--settle-dur, 34s) ease-in-out infinite;
    transform-box: view-box;
    transform-origin: 50% 50%;
  }

  .knot use {
    fill: none;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .knot .base,
  .knot .over {
    stroke: var(--knot-line);
    stroke-width: var(--sw, 2.6px);
  }

  .knot .pulse {
    stroke: var(--pulse, var(--pulse-idle));
  }
  .knot .p1 {
    stroke-dasharray: 16 84;
    stroke-width: calc(var(--sw, 2.6px) * 1.7);
    opacity: 0.1;
    animation: knot-lap1 var(--lap-a, 11s) linear infinite;
    animation-delay: var(--seed, 0s);
  }
  .knot .p2 {
    stroke-dasharray: 10 90;
    stroke-width: calc(var(--sw, 2.6px) * 1.2);
    opacity: 0.22;
    animation: knot-lap2 var(--lap-a, 11s) linear infinite;
    animation-delay: var(--seed, 0s);
  }
  .knot .p3 {
    stroke-dasharray: 4 96;
    stroke-width: var(--sw, 2.6px);
    opacity: var(--head-op, 0.55);
    animation: knot-lap3 var(--lap-a, 11s) linear infinite;
    animation-delay: var(--seed, 0s);
  }
  .knot .pb {
    stroke-dasharray: 8 92;
    stroke-width: var(--sw, 2.6px);
    opacity: 0.13;
    animation: knot-lapR var(--lap-b, 26s) linear infinite;
    animation-delay: var(--seed, 0s);
  }

  .knot .heldg {
    display: none;
  }
  .knot .heldg use {
    stroke: var(--blocked);
  }
  .knot .heldg .hg {
    stroke-width: calc(var(--sw, 2.6px) * 2.6);
    opacity: 0.18;
  }
  .knot .heldg .hc {
    stroke-width: var(--sw, 2.6px);
  }

  /* states */
  .knot.st-local {
    --pulse: var(--local);
    --lap-a: 2.4s;
    --lap-b: 5.2s;
    --head-op: 0.95;
  }
  .knot.st-cloud {
    --pulse: var(--cloud);
    --lap-a: 2.4s;
    --lap-b: 5.2s;
    --head-op: 0.95;
  }
  .knot.st-filter {
    --pulse: var(--warn);
    --lap-a: 2.4s;
    --lap-b: 5.2s;
    --head-op: 0.95;
  }
  .knot.st-local .base,
  .knot.st-local .over,
  .knot.st-cloud .base,
  .knot.st-cloud .over,
  .knot.st-filter .base,
  .knot.st-filter .over {
    stroke: var(--pulse);
    opacity: 0.8;
  }
  .knot.st-local .p1,
  .knot.st-cloud .p1,
  .knot.st-filter .p1 {
    opacity: 0.16;
  }
  .knot.st-local .p2,
  .knot.st-cloud .p2,
  .knot.st-filter .p2 {
    opacity: 0.34;
  }
  .knot.st-local .pb,
  .knot.st-cloud .pb,
  .knot.st-filter .pb {
    opacity: 0.2;
  }
  .knot.st-held .pulse {
    display: none;
  }
  .knot.st-held .heldg {
    display: inline;
    animation: knot-heldPulse 2.8s ease-in-out infinite;
  }
  .knot.st-held .base,
  .knot.st-held .over {
    opacity: 0.75;
  }

  @keyframes knot-breathe {
    0%,
    100% {
      transform: scale(1);
    }
    50% {
      transform: scale(1.022);
    }
  }
  @keyframes knot-settle {
    0%,
    37% {
      transform: rotate(0deg);
    }
    39.5% {
      transform: rotate(0.7deg);
    }
    42%,
    73% {
      transform: rotate(0deg);
    }
    75.5% {
      transform: rotate(-0.5deg);
    }
    78%,
    100% {
      transform: rotate(0deg);
    }
  }
  @keyframes knot-lap1 {
    from {
      stroke-dashoffset: 0;
    }
    to {
      stroke-dashoffset: -100;
    }
  }
  @keyframes knot-lap2 {
    from {
      stroke-dashoffset: -6;
    }
    to {
      stroke-dashoffset: -106;
    }
  }
  @keyframes knot-lap3 {
    from {
      stroke-dashoffset: -12;
    }
    to {
      stroke-dashoffset: -112;
    }
  }
  @keyframes knot-lapR {
    from {
      stroke-dashoffset: 0;
    }
    to {
      stroke-dashoffset: 100;
    }
  }
  @keyframes knot-heldPulse {
    0%,
    100% {
      opacity: 0.45;
    }
    50% {
      opacity: 1;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .knot .breathe,
    .knot .settle,
    .knot .pulse,
    .knot .heldg {
      animation: none !important;
    }
    .knot .pulse {
      opacity: 0 !important;
    }
  }
</style>
