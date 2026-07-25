<script lang="ts">
  // GmailSetupWizard — the guided "bring your own Google OAuth client" setup
  // (M7-Q2: every user creates their OWN Google Cloud OAuth client; no vendor
  // client, no Lost Harness server in the loop). Steps 1–5 are install-global
  // (the pasted client id/secret are shared by every profile on this machine);
  // step 6 — Connect — is per-profile. The `reconnect` variant skips straight
  // to Connect with calm copy: Testing-status Google clients expire their
  // refresh tokens about every 7 days, which is routine, not an error.
  // Visual language follows the Onboarding mockup's step/dot rail.
  import { untrack } from "svelte";
  import Button from "./Button.svelte";
  import {
    setGmailClient,
    gmailBeginConnect,
    gmailFinishConnect,
    type GmailSetupStatus,
  } from "$lib/api/tauri";

  interface Props {
    /** Profile that Connect binds — each profile connects its own inbox. */
    profile: string;
    status: GmailSetupStatus;
    /** "reconnect": compact card, straight to Connect, calm expiry copy. */
    variant?: "full" | "reconnect";
    /** Fired after a successful connect so the screen can re-check status. */
    onconnected?: () => void;
  }

  let { profile, status, variant = "full", onconnected }: Props = $props();

  const STEPS = ["Why", "Project", "Gmail API", "Consent", "Client", "Connect"] as const;
  const CONNECT_STEP = STEPS.length - 1;

  // The furthest-reached step persists per install (the client steps are
  // install-global), so quitting mid-setup resumes where the user left off.
  const FURTHEST_KEY = "lh.gmailWizard.furthest.v1";
  function readFurthest(): number {
    try {
      const raw = localStorage.getItem(FURTHEST_KEY);
      const n = raw == null ? 0 : parseInt(raw, 10);
      return Number.isFinite(n) ? Math.min(Math.max(n, 0), CONNECT_STEP) : 0;
    } catch {
      return 0;
    }
  }
  function saveFurthest(n: number) {
    try {
      if (n > readFurthest()) localStorage.setItem(FURTHEST_KEY, String(n));
    } catch {
      // localStorage may be unavailable — resume is best-effort.
    }
  }

  // A configured client means steps 1–5 are done for this install: jump
  // straight to Connect. Otherwise resume at the furthest step reached.
  // Deliberately the INITIAL value only (untrack): the Email screen remounts
  // the wizard via {#key} whenever the profile or variant changes.
  let step = $state(
    untrack(() =>
      variant === "reconnect" || status.client_configured ? CONNECT_STEP : readFurthest(),
    ),
  );

  function advance() {
    step = Math.min(step + 1, CONNECT_STEP);
    saveFurthest(step);
  }
  function back() {
    step = Math.max(step - 1, 0);
  }

  // ── step 5 (Client) — the two paste fields ─────────────────────────────────
  let clientId = $state("");
  let clientSecret = $state("");
  let clientSaving = $state(false);
  let clientError: string | null = $state(null);
  const clientIdValid = $derived(clientId.trim().endsWith(".apps.googleusercontent.com"));
  const clientSaveEnabled = $derived(
    clientIdValid && clientSecret.trim().length > 0 && !clientSaving,
  );

  // Format client configuration errors for clarity
  function formatClientError(err: unknown): string {
    const msg = String(err);
    // Backend validation errors typically mention format or invalid credentials
    if (msg.toLowerCase().includes("invalid") || msg.toLowerCase().includes("malformed")) {
      return "Check your Google client credentials. Make sure the client ID and secret are correct and match your Google Cloud project.";
    }
    if (msg.toLowerCase().includes("credential") || msg.toLowerCase().includes("secret")) {
      return "The client secret appears invalid. Double-check it was copied completely from the Google Cloud console.";
    }
    return msg;
  }

  async function saveClient() {
    if (!clientSaveEnabled) return;
    clientSaving = true;
    clientError = null;
    try {
      // Backend re-validates the id format and stores both in the keychain.
      await setGmailClient(clientId.trim(), clientSecret.trim());
      advance();
    } catch (err) {
      clientError = formatClientError(err);
    } finally {
      clientSaving = false;
    }
  }

  // ── step 6 (Connect) ───────────────────────────────────────────────────────
  let connectPhase = $state<"idle" | "waiting" | "done">("idle");
  let authUrl: string | null = $state(null);
  let connectError: string | null = $state(null);
  let connectedAs: string | null = $state(null);

  // Distinguish reconnect (stale token) from misconfiguration (bad client setup)
  function formatConnectError(err: unknown): string {
    const msg = String(err);
    // Backend error messages distinguish these cases; surface the right guidance
    if (msg.toLowerCase().includes("reconnect") || msg.toLowerCase().includes("token")) {
      return "Gmail needs a quick reconnect — your access token expired. Click Connect again to re-authenticate.";
    }
    if (msg.toLowerCase().includes("client") || msg.toLowerCase().includes("credential")) {
      return "Check your Google client credentials. Make sure the client ID and secret are correct and match your Google Cloud project.";
    }
    // Generic fallback for other errors
    return msg;
  }

  async function connect() {
    connectPhase = "waiting";
    connectError = null;
    authUrl = null;
    try {
      const begun = await gmailBeginConnect(profile);
      authUrl = begun.auth_url;
      const done = await gmailFinishConnect(profile);
      connectedAs = done.account_email;
      connectPhase = "done";
      saveFurthest(CONNECT_STEP);
      onconnected?.();
    } catch (err) {
      connectError = formatConnectError(err);
      connectPhase = "idle";
    }
  }

  const card = "rounded-[var(--r-lg)] border border-border bg-surface p-5 text-left";
  const heading = "mb-1.5 text-[13px] font-semibold";
  const body = "mb-3 text-[12.5px] leading-[1.55] text-text-2";
  const note =
    "rounded-[var(--r)] border border-border bg-surface-2 px-3 py-2.5 text-[12px] leading-[1.55] text-text-2";
  const linkBtn =
    "inline-flex cursor-pointer items-center gap-1.5 rounded-[var(--r)] border border-border-strong bg-surface px-[13px] py-[7px] text-[12.5px] font-semibold text-text no-underline transition hover:bg-surface-hover";
  const inputCls =
    "w-full rounded-[var(--r)] border border-border bg-surface-2 px-[11px] py-[9px] font-mono text-[12.5px] text-text outline-none placeholder:font-sans placeholder:text-text-3";
</script>

{#snippet externalIcon()}
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0">
    <path d="M14 4h6v6M20 4l-9 9" />
    <path d="M10 5H6a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2v-4" />
  </svg>
{/snippet}

<!-- Shared Connect innards — used by step 6 and the reconnect variant. -->
{#snippet connectBody()}
  {#if connectPhase === "done"}
    <div
      class="flex items-center gap-2 rounded-[var(--r)] bg-local-soft px-3 py-2.5 text-[12.5px] font-[550] text-local"
    >
      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" class="shrink-0">
        <path d="M4 12l5 5L20 7" />
      </svg>
      Connected as {connectedAs}
    </div>
  {:else}
    <div class="flex items-center gap-2.5">
      <Button
        variant="primary"
        disabled={connectPhase === "waiting"}
        onclick={() => void connect()}
      >
        {connectPhase === "waiting" ? "Waiting for Google…" : "Connect Gmail"}
      </Button>
      {#if connectPhase === "waiting"}
        <span class="text-[12px] text-text-3">finish the sign-in in your browser</span>
      {/if}
    </div>
    {#if authUrl && connectPhase === "waiting"}
      <div class="mt-3 rounded-[var(--r)] border border-border bg-surface-2 px-3 py-2.5">
        <div class="mb-1 text-[11px] text-text-3">
          If the browser didn't open, paste this into it:
        </div>
        <code class="block select-all break-all font-mono text-[11px] leading-[1.5] text-text-2">
          {authUrl}
        </code>
      </div>
    {/if}
    {#if connectError}
      <div class="mt-2.5 text-[12.5px] text-red-400">{connectError}</div>
    {/if}
  {/if}
{/snippet}

{#if variant === "reconnect"}
  <div class={card}>
    <div class={heading}>Reconnect Gmail</div>
    <p class={body}>
      Gmail needs a quick reconnect — routine for personal Google clients. In
      Testing status, Google expires the connection about every 7 days;
      reconnecting is one click. To stop the expiry, publish your consent
      screen to Production in the Google Cloud console.
    </p>
    {@render connectBody()}
  </div>
{:else}
  <div class="flex flex-col">
    <div class="pb-4 text-center">
      <h1 class="mb-1 text-[18px] font-[650] tracking-[-0.01em]">Set up Gmail</h1>
      <p class="mx-auto max-w-[460px] text-[12.5px] leading-[1.55] text-text-2">
        Your mail flows through your own private Google connection — Lost
        Harness never runs a server in between.
      </p>
    </div>

    <!-- step rail (the Onboarding pattern) -->
    <div class="flex flex-wrap items-center justify-center gap-2 pb-5">
      {#each STEPS as label, i (label)}
        {#if i > 0}
          <span class="h-px w-3 bg-border-strong"></span>
        {/if}
        <div
          class="flex items-center gap-[6px] text-[11px] {i <= step ? 'text-text' : 'text-text-3'}"
        >
          <span
            class="grid h-5 w-5 place-items-center rounded-full border text-[10.5px] font-[650] {i <= step
              ? 'border-transparent bg-accent text-on-accent'
              : 'border-border bg-surface-2'}"
          >
            {i + 1}
          </span>
          {label}
        </div>
      {/each}
    </div>

    <!-- current step card -->
    {#if step === 0}
      <div class={card}>
        <div class={heading}>Why this setup exists</div>
        <p class={body}>
          Lost Harness never runs a server — nothing sits between you and
          Google. Instead, you create your own private Google connection: an
          OAuth client that belongs to you alone, in your own Google Cloud
          account. Nobody else ever sees it, and your mail never touches
          anyone's infrastructure but Google's and this machine's.
        </p>
        <p class="text-[12.5px] leading-[1.55] text-text-2">
          It takes about 5–10 minutes, once per install. After that, each
          profile connects its own inbox with a single click.
        </p>
      </div>
    {:else if step === 1}
      <div class={card}>
        <div class={heading}>Create a Google Cloud project</div>
        <p class={body}>
          Sign in with your Google account and create a project — the name
          doesn't matter (something like "Lost Harness" works). This is free;
          the Gmail API needs no billing setup.
        </p>
        <a
          class={linkBtn}
          href="https://console.cloud.google.com/projectcreate"
          target="_blank"
          rel="noopener noreferrer"
        >
          Open the project-create page
          {@render externalIcon()}
        </a>
      </div>
    {:else if step === 2}
      <div class={card}>
        <div class={heading}>Enable the Gmail API</div>
        <p class={body}>
          With your new project selected (the picker in the console's top bar),
          open the Gmail API page and click <b>Enable</b>.
        </p>
        <a
          class={linkBtn}
          href="https://console.cloud.google.com/apis/library/gmail.googleapis.com"
          target="_blank"
          rel="noopener noreferrer"
        >
          Open the Gmail API page
          {@render externalIcon()}
        </a>
      </div>
    {:else if step === 3}
      <div class={card}>
        <div class={heading}>Configure the consent screen</div>
        <p class={body}>
          Choose <b>External</b>, fill only the required fields (app name +
          your email twice), and under Test users add <b>your own email</b> —
          the account whose mail you'll connect.
        </p>
        <div class="mb-3 {note}">
          Google will show a one-time "unverified app" screen — that's
          expected: this client belongs to you, nobody else ever sees it. In
          Testing status Google also expires the connection every ~7 days;
          reconnecting is one click, or publish the consent screen to
          Production to stop that.
        </div>
        <a
          class={linkBtn}
          href="https://console.cloud.google.com/apis/credentials/consent"
          target="_blank"
          rel="noopener noreferrer"
        >
          Open the consent-screen page
          {@render externalIcon()}
        </a>
      </div>
    {:else if step === 4}
      <div class={card}>
        <div class={heading}>Create the OAuth client</div>
        <p class={body}>
          Application type <b>Desktop app</b> (any name). Google then shows a
          client ID and a client secret — paste both here. They're stored in
          this machine's keychain, shared by all profiles on this install.
        </p>
        <a
          class="{linkBtn} mb-3"
          href="https://console.cloud.google.com/apis/credentials/oauthclient"
          target="_blank"
          rel="noopener noreferrer"
        >
          Open the create-client page
          {@render externalIcon()}
        </a>
        <div class="flex flex-col gap-2">
          <div>
            <label class="mb-1 block text-[11px] font-semibold text-text-2" for="gmail-client-id">
              Client ID
            </label>
            <input
              id="gmail-client-id"
              bind:value={clientId}
              placeholder="…….apps.googleusercontent.com"
              spellcheck="false"
              autocomplete="off"
              class={inputCls}
            />
            {#if clientId.trim() !== "" && !clientIdValid}
              <div class="mt-1 text-[11.5px] text-red-400">
                A Google OAuth client ID ends with .apps.googleusercontent.com
              </div>
            {/if}
          </div>
          <div>
            <label
              class="mb-1 block text-[11px] font-semibold text-text-2"
              for="gmail-client-secret"
            >
              Client secret
            </label>
            <input
              id="gmail-client-secret"
              bind:value={clientSecret}
              placeholder="GOCSPX-…"
              spellcheck="false"
              autocomplete="off"
              class={inputCls}
            />
          </div>
          {#if clientError}
            <div class="text-[12.5px] text-red-400">{clientError}</div>
          {/if}
          <div>
            <Button variant="primary" disabled={!clientSaveEnabled} onclick={() => void saveClient()}>
              {clientSaving ? "Saving…" : "Save client"}
            </Button>
          </div>
        </div>
      </div>
    {:else}
      <div class={card}>
        <div class={heading}>Connect Gmail</div>
        <p class={body}>
          Each profile connects its own inbox — this connects the
          <b>{profile}</b> profile. Your browser opens a Google sign-in;
          approve the access and come back. The one-time "unverified app"
          screen is expected — the client is yours alone.
        </p>
        {@render connectBody()}
      </div>
    {/if}

    <!-- footer nav -->
    <div class="flex items-center pt-3">
      {#if step > 0}
        <Button variant="ghost" onclick={back}>Back</Button>
      {/if}
      <div class="flex-1"></div>
      {#if step <= 3}
        <Button variant="primary" onclick={advance}>
          {step === 0 ? "Got it — start" : "Done — next"}
        </Button>
      {/if}
    </div>
  </div>
{/if}
