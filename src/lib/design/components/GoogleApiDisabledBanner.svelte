<script lang="ts">
  // The SECOND recoverable Google 403 — the one Reconnect cannot fix.
  //
  // A Google API call can fail two recoverable ways. If the stored grant is
  // missing a scope, reconnecting genuinely fixes it and the calm reconnect
  // strip is the right answer. If the API is switched OFF in the user's own
  // Google Cloud project, reconnecting re-consents the same scopes against the
  // same project and fails identically — so offering Reconnect here would be
  // an infinite loop with no exit. This banner is the other exit: it says what
  // Google actually refused, points at the console, and never offers a
  // reconnect.
  //
  // Deliberately calm and grayscale (CONVENTIONS: saturated color is reserved
  // for the privacy/routing signal). It reads as distinct from the reconnect
  // strip by SHAPE — a titled block with an icon and two actions, not a
  // one-line strip.
  import Button from "./Button.svelte";
  import type { DisabledApiLink } from "$lib/design/googleConnection.svelte";

  interface Props {
    /** Which APIs answered "switched off", each with the activation link
     *  Google gave FOR THAT API (validated backend-side; `null` when the
     *  response carried none).
     *
     *  One entry per API, not a label list plus one shared link: with Calendar
     *  and Tasks both off, Planner named two APIs and could only offer the
     *  first one's page — so the second was unreachable from the banner that
     *  named it. The backend has always sent them separately; the banner now
     *  keeps them that way. */
    apis?: DisabledApiLink[];
    /** "I've enabled it — check again": forget the state and retry. */
    oncheckagain: () => void;
    /** A retry is in flight. */
    checking?: boolean;
  }

  let { apis = [], oncheckagain, checking = false }: Props = $props();

  /** "Google Tasks", "Gmail and Google Tasks", "…, … and …". */
  function list(labels: string[]): string {
    return labels.length === 1
      ? labels[0]
      : `${labels.slice(0, -1).join(", ")} and ${labels[labels.length - 1]}`;
  }

  /** The APIs this banner names. Falls back to the vaguer phrasing only when
   *  we genuinely weren't told which. */
  const named = $derived(apis.length === 0 ? null : list(apis.map((a) => a.label)));

  /** The ones Google pointed somewhere for — each gets its own button. */
  const linked = $derived(
    apis.flatMap((a) =>
      a.console_url == null ? [] : [{ label: a.label, url: a.console_url }],
    ),
  );
  /** The ones it didn't — the API library covers all of them at once. */
  const unlinked = $derived(apis.filter((a) => a.console_url == null));

  /** Name the link's API whenever there is more than one destination on the
   *  banner; with a single one there is nothing to disambiguate. */
  const oneDestination = $derived(linked.length === 1 && unlinked.length === 0);

  /** Show the generic library link when some API here has no link of its own —
   *  and when we weren't told which APIs at all. */
  const showLibrary = $derived(unlinked.length > 0 || apis.length === 0);

  // The same static pointer the setup wizard hands out at its "enable the
  // APIs" step. Used ONLY for the APIs Google gave no link of its own for — a
  // known-good page of ours, never a URL guessed from the error text.
  const API_LIBRARY = "https://console.cloud.google.com/apis/library";

  const linkBtn =
    "inline-flex cursor-pointer items-center gap-1.5 rounded-[var(--r)] border border-border-strong bg-surface px-[13px] py-[7px] text-[12.5px] font-semibold text-text no-underline transition hover:bg-surface-hover";
</script>

<div
  class="flex flex-shrink-0 items-start gap-2.5 border-b border-l-2 border-border border-l-border-strong bg-surface px-[18px] py-2.5"
  role="status"
  aria-live="polite"
  data-testid="google-api-disabled-banner"
>
  <svg
    width="15"
    height="15"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.8"
    class="mt-0.5 shrink-0 text-text-3"
    aria-hidden="true"
  >
    <circle cx="12" cy="12" r="9" />
    <path d="M12 7.5v5" stroke-linecap="round" />
    <circle cx="12" cy="16.3" r="0.9" fill="currentColor" stroke="none" />
  </svg>
  <div class="min-w-0 flex-1">
    <div class="text-[12.5px] font-semibold text-text">
      {named == null ? "A Google API isn't switched on" : `${named} isn't switched on`}
    </div>
    <p class="mt-0.5 text-[12px] leading-[1.5] text-text-2">
      Google turned this request down because
      {named == null
        ? "one of the APIs this profile uses — Gmail, Calendar or Tasks — is"
        : `${named} ${apis.length > 1 ? "are" : "is"}`}
      switched off in your Google Cloud project.
      Reconnecting won't help: the switch lives in the console, not in the
      permission you granted. Turn it on there, then check again.
      {#if showLibrary}
        Google didn't include a direct link
        {#if unlinked.length > 0 && linked.length > 0}for {list(
            unlinked.map((a) => a.label),
          )}{:else}this time{/if} — open the API library, pick the project you
        made during setup, and enable what you need.
      {/if}
    </p>
    <div class="mt-2 flex flex-wrap items-center gap-2">
      <!-- One button per API Google gave a page for, plus at most one library
           fallback covering the ones it didn't. -->
      {#each linked as api (api.label)}
        {@render consoleLink(
          api.url,
          oneDestination
            ? "Open the page Google pointed to"
            : `Open the page for ${api.label}`,
        )}
      {/each}
      {#if showLibrary}
        {@render consoleLink(API_LIBRARY, "Open the API library")}
      {/if}
      <Button variant="ghost" disabled={checking} onclick={oncheckagain}>
        {checking ? "Checking…" : "I've enabled it — check again"}
      </Button>
    </div>
  </div>
</div>

{#snippet consoleLink(href: string, label: string)}
  <a
    class={linkBtn}
    {href}
    target="_blank"
    rel="noopener noreferrer"
    data-testid="google-api-console-link"
  >
    {label}
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      class="shrink-0"
      aria-hidden="true"
    >
      <path d="M14 4h6v6M20 4l-9 9" />
      <path d="M10 5H6a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2v-4" />
    </svg>
  </a>
{/snippet}
