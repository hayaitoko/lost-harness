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

  interface Props {
    /** Google's own activation link, validated backend-side. `null` when the
     *  response carried none — we then point at the console generally rather
     *  than inventing a per-API URL. */
    consoleUrl: string | null;
    /** "I've enabled it — check again": forget the state and retry. */
    oncheckagain: () => void;
    /** A retry is in flight. */
    checking?: boolean;
  }

  let { consoleUrl, oncheckagain, checking = false }: Props = $props();

  // The same static pointer the setup wizard hands out at its "enable the
  // APIs" step. Used ONLY when Google gave no link of its own — a known-good
  // page of ours, never a URL guessed from the error text.
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
      A Google API isn't switched on
    </div>
    <p class="mt-0.5 text-[12px] leading-[1.5] text-text-2">
      Google turned this request down because one of the APIs this profile uses
      — Gmail, Calendar or Tasks — is switched off in your Google Cloud project.
      Reconnecting won't help: the switch lives in the console, not in the
      permission you granted. Turn it on there, then check again.
      {#if consoleUrl == null}
        Google didn't include a direct link this time — open the API library,
        pick the project you made during setup, and enable the one you need.
      {/if}
    </p>
    <div class="mt-2 flex flex-wrap items-center gap-2">
      <a
        class={linkBtn}
        href={consoleUrl ?? API_LIBRARY}
        target="_blank"
        rel="noopener noreferrer"
        data-testid="google-api-console-link"
      >
        {consoleUrl == null
          ? "Open the API library"
          : "Open the page Google pointed to"}
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
          <path
            d="M10 5H6a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2v-4"
          />
        </svg>
      </a>
      <Button variant="ghost" disabled={checking} onclick={oncheckagain}>
        {checking ? "Checking…" : "I've enabled it — check again"}
      </Button>
    </div>
  </div>
</div>
