<script lang="ts">
  // Email — the LIVE Gmail surface for the active profile. When the profile
  // isn't connected it renders the guided setup wizard (per-user OAuth client,
  // M7-Q2 — the client steps are install-global, Connect is per-profile);
  // when connected it's an honest inbox: list via `list_email`, reading pane
  // via `read_email` (escaped plain text — mail content is untrusted, never
  // {@html}), compose → `send_email`. The old mockup's fake multi-account
  // switcher, labels, and providers row are gone: Gmail only today, and the
  // screen says so. `needs_reconnect` surfaces as a calm banner, not an error
  // (Testing-status Google clients expire tokens ~every 7 days).
  //
  // TWO connection banners, never one: a scope-short grant is fixed by
  // reconnecting, a Google API switched off in the user's Cloud project is
  // not. Sharing one banner would offer Reconnect for a condition reconnecting
  // cannot change — an infinite loop. See `GoogleApiDisabledBanner`.
  import { nav } from "$lib/design/nav.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import GmailSetupWizard from "../components/GmailSetupWizard.svelte";
  import GoogleApiDisabledBanner from "../components/GoogleApiDisabledBanner.svelte";
  import { activeProfileId } from "$lib/stores/profiles";
  import {
    gmailSetupStatus,
    gmailDisconnect,
    googleClearApiNotEnabled,
    listEmail,
    readEmail,
    sendEmail,
    type GmailSetupStatus,
    type EmailSummary,
    type EmailDetail,
  } from "$lib/api/tauri";

  // ── connection status (per profile) ────────────────────────────────────────
  let status = $state<GmailSetupStatus | null>(null);
  let statusError: string | null = $state(null);
  let statusTick = $state(0);
  let statusSeq = 0;

  // ── inbox list ─────────────────────────────────────────────────────────────
  let emails = $state<EmailSummary[]>([]);
  let listLoading = $state(false);
  let listError: string | null = $state(null);
  let listSeq = 0;
  let listTick = $state(0);

  // ── reading pane ───────────────────────────────────────────────────────────
  let selectedId: string | null = $state(null);
  let detail = $state<EmailDetail | null>(null);
  let detailLoading = $state(false);
  let detailError: string | null = $state(null);
  let detailSeq = 0;

  // ── header actions ─────────────────────────────────────────────────────────
  let confirmDisconnect = $state(false);
  let actionError: string | null = $state(null);
  let reconnecting = $state(false);
  let recheckingApi = $state(false);

  // Has the CONNECTION state (as opposed to the mail) changed? Both recoverable
  // Google failures count, because both drive a banner.
  function connectionStateChanged(a: GmailSetupStatus, b: GmailSetupStatus): boolean {
    return (
      a.needs_reconnect !== b.needs_reconnect ||
      (a.api_not_enabled == null) !== (b.api_not_enabled == null) ||
      (a.api_not_enabled?.console_url ?? null) !== (b.api_not_enabled?.console_url ?? null)
    );
  }

  // One effect owns the status check + the profile-switch reset (the
  // connection is per-profile, so switching drops the old profile's mail
  // state). `seq` tokens drop stale responses, like Files.svelte.
  let lastProfile: string | null = null;
  $effect(() => {
    const profile = $activeProfileId;
    void statusTick; // manual re-check hook (connect / disconnect / reconnect)
    if (profile !== lastProfile) {
      lastProfile = profile;
      status = null; // gate the UI on a fresh check for the new profile
      emails = [];
      selectedId = null;
      detail = null;
      detailError = null;
      listError = null;
      listLoading = false; // clear loading flag on profile switch
      actionError = null;
      reconnecting = false;
      recheckingApi = false;
      confirmDisconnect = false;
      listSeq++;
      detailSeq++; // invalidate anything in flight for the old profile
    }
    const token = ++statusSeq;
    statusError = null;
    gmailSetupStatus(profile)
      .then((s) => {
        if (token === statusSeq) status = s;
      })
      .catch((err) => {
        if (token === statusSeq) {
          statusError = String(err);
          status = null;
        }
      });
  });

  // Inbox listing — only when connected and the grant isn't known-dead
  // (attempting on a dead grant just spams failing token refreshes).
  //
  // NOT gated on `api_not_enabled`: that state is per-profile but its cause is
  // per-API, so a disabled Tasks API (recorded by the Planner) must not stop
  // the inbox from loading when Gmail itself is fine. If Gmail IS the disabled
  // one, the attempt fails, the banner lights, and the re-check below finds no
  // further change — so it settles rather than looping.
  $effect(() => {
    const profile = $activeProfileId;
    void listTick;
    const s = status;
    if (!s?.connected || s.needs_reconnect) return;
    const token = ++listSeq;
    const statusToken = statusSeq; // capture status token to gate secondary status writes
    listLoading = true;
    listError = null;
    listEmail(profile)
      .then((rows) => {
        if (token === listSeq) emails = rows;
      })
      .catch(async (err) => {
        if (token !== listSeq) return;
        listError = String(err);
        // A dead grant or a disabled API is recorded backend-side — re-check
        // once so the matching calm banner appears. Only swap status when the
        // connection state actually changed; the effect then settles on rerun
        // (no retry loop).
        try {
          const fresh = await gmailSetupStatus(profile);
          if (
            token === listSeq &&
            statusToken === statusSeq &&
            status &&
            connectionStateChanged(fresh, status)
          ) {
            status = fresh;
          }
        } catch {
          // keep the list error
        }
      })
      .finally(() => {
        if (token === listSeq) listLoading = false;
      });
  });

  async function openEmail(id: string) {
    selectedId = id;
    const token = ++detailSeq;
    detailLoading = true;
    detailError = null;
    detail = null;
    try {
      const d = await readEmail($activeProfileId, id);
      if (token === detailSeq) detail = d;
    } catch (err) {
      if (token === detailSeq) detailError = String(err);
    } finally {
      if (token === detailSeq) detailLoading = false;
    }
  }

  // Two-click confirm (the ScheduledJobs delete pattern).
  async function disconnect() {
    if (!confirmDisconnect) {
      confirmDisconnect = true;
      setTimeout(() => (confirmDisconnect = false), 3000);
      return;
    }
    confirmDisconnect = false;
    actionError = null;
    try {
      await gmailDisconnect($activeProfileId);
      emails = [];
      selectedId = null;
      detail = null;
      detailError = null;
      listError = null;
      reconnecting = false;
      statusTick++;
    } catch (err) {
      actionError = String(err);
    }
  }

  // ── compose ────────────────────────────────────────────────────────────────
  let composeOpen = $state(false);
  let composeTo = $state("");
  let composeSubject = $state("");
  let composeBody = $state("");
  let sending = $state(false);
  let sendError: string | null = $state(null);
  let sentTo: string | null = $state(null);

  function openCompose() {
    composeOpen = true;
    composeTo = "";
    composeSubject = "";
    composeBody = "";
    sendError = null;
    sentTo = null;
  }
  const canSend = $derived(composeTo.trim().length > 0 && !sending);
  async function doSend() {
    if (!canSend) return;
    sending = true;
    sendError = null;
    try {
      await sendEmail($activeProfileId, composeTo.trim(), composeSubject.trim(), composeBody);
      sentTo = composeTo.trim();
    } catch (err) {
      sendError = String(err);
    } finally {
      sending = false;
    }
  }

  const showWizard = $derived(
    status != null && (!status.client_configured || !status.connected || reconnecting),
  );
  const wizardVariant = $derived(
    status?.client_configured && status?.needs_reconnect ? ("reconnect" as const) : ("full" as const),
  );
  const showBanner = $derived(
    status != null && status.connected && status.needs_reconnect && !reconnecting,
  );
  // The OTHER recoverable 403. Separate banner, separate condition, no
  // Reconnect button and no wizard — reconnecting cannot enable a disabled API.
  const apiDisabled = $derived(
    status != null && status.connected && !reconnecting ? status.api_not_enabled : null,
  );

  // "I've enabled it — check again": forget the sticky state, then retry. If
  // the API is still off the next call re-records it and the banner returns —
  // nothing is assumed fixed.
  async function recheckApi() {
    if (recheckingApi) return;
    recheckingApi = true;
    actionError = null;
    try {
      await googleClearApiNotEnabled($activeProfileId);
    } catch (err) {
      actionError = String(err);
    } finally {
      recheckingApi = false;
    }
    statusTick++;
    listTick++;
  }

  function fmtDate(raw: string): string {
    const d = new Date(raw);
    if (Number.isNaN(d.getTime())) return raw;
    return d.toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
    });
  }
  function subjectOr(s: string): string {
    return s.trim() === "" ? "(no subject)" : s;
  }

  const modalInput =
    "w-full rounded-[var(--r)] border border-border bg-surface-2 px-[11px] py-[9px] text-[13px] text-text outline-none placeholder:text-text-3";
</script>

<div class="grid h-screen" style="grid-template-columns:260px 1fr 0">
  <Sidebar active="email" />

  <main class="flex min-w-0 min-h-0 flex-col">
    <div
      class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]"
    >
      <div class="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap text-[13.5px] font-semibold">
        Email
      </div>
      <span class="min-w-0 truncate text-[11.5px] text-text-3">
        {#if status?.connected}
          connected as {status.account_email ?? "your Gmail account"} — the {$activeProfileId} profile's inbox
        {:else}
          the {$activeProfileId} profile's inbox — not connected yet
        {/if}
      </span>
      <div class="flex-1"></div>
      <div class="flex flex-shrink-0 items-center gap-1.5">
        {#if status?.connected}
          <button
            type="button"
            onclick={() => void disconnect()}
            class="inline-flex cursor-pointer items-center rounded-[var(--r)] border px-[11px] py-[5px] text-[11.5px] font-semibold transition {confirmDisconnect
              ? 'border-blocked bg-blocked-soft text-blocked'
              : 'border-border-strong bg-surface text-text-2 hover:bg-surface-hover'}"
          >
            {confirmDisconnect ? "Disconnect?" : "Disconnect"}
          </button>
          <Button variant="primary" onclick={openCompose}>Compose</Button>
        {/if}
        <IconButton label="Settings" onclick={() => nav.go("settings")}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M4 7h16M4 12h16M4 17h16" />
            <circle cx="9" cy="7" r="2" fill="var(--surface)" />
            <circle cx="15" cy="12" r="2" fill="var(--surface)" />
            <circle cx="8" cy="17" r="2" fill="var(--surface)" />
          </svg>
        </IconButton>
      </div>
    </div>

    {#if showBanner}
      <!-- calm reconnect banner — routine, not an error -->
      <div class="flex flex-shrink-0 items-center gap-3 border-b border-border bg-surface-2 px-[18px] py-2">
        <span class="text-[12px] text-text-2">
          Gmail needs a quick reconnect — routine for personal Google clients.
        </span>
        <div class="flex-1"></div>
        <Button onclick={() => (reconnecting = true)}>Reconnect</Button>
      </div>
    {/if}

    {#if apiDisabled}
      <GoogleApiDisabledBanner
        consoleUrl={apiDisabled.console_url}
        checking={recheckingApi}
        oncheckagain={() => void recheckApi()}
      />
    {/if}

    {#if actionError}
      <div class="border-b border-border px-[18px] py-2 text-[12.5px] text-red-400">
        {actionError}
      </div>
    {/if}

    {#if statusError}
      <div class="min-h-0 flex-1 overflow-y-auto">
        <div class="mx-auto max-w-[640px] px-6 pb-12 pt-[22px]">
          <div class="mb-3 text-[12.5px] text-red-400">{statusError}</div>
          <Button onclick={() => statusTick++}>Try again</Button>
        </div>
      </div>
    {:else if status == null}
      <div class="min-h-0 flex-1 overflow-y-auto">
        <div class="px-6 py-6 text-[12.5px] text-text-3">Checking Gmail setup…</div>
      </div>
    {:else if showWizard}
      <div class="min-h-0 flex-1 overflow-y-auto">
        <div class="mx-auto max-w-[640px] px-6 pb-12 pt-[22px]">
          {#if reconnecting && status.connected}
            <div class="pb-3">
              <Button variant="ghost" onclick={() => (reconnecting = false)}>← Back to inbox</Button>
            </div>
          {/if}
          {#key `${$activeProfileId}:${wizardVariant}`}
            <GmailSetupWizard
              profile={$activeProfileId}
              {status}
              variant={wizardVariant}
              onconnected={() => {
                reconnecting = false;
                statusTick++;
              }}
            />
          {/key}
          <p class="mt-6 px-1 text-center text-[11.5px] text-text-3">
            Gmail only today. Other providers later — the connection layer is
            per-account by design, and each profile connects its own inbox.
          </p>
        </div>
      </div>
    {:else}
      <!-- two-pane body: inbox list + reading pane -->
      <div class="grid min-h-0 flex-1" style="grid-template-columns:320px minmax(0,1fr)">
        <!-- list column -->
        <div class="flex min-h-0 min-w-0 flex-col border-r border-border">
          <div class="flex flex-shrink-0 items-center gap-2 px-3 pb-1 pt-2">
            <span class="text-[10px] font-semibold uppercase tracking-[0.06em] text-text-3">
              Inbox
            </span>
            {#if !listLoading && emails.length > 0}
              <span class="text-[10.5px] text-text-3">latest {emails.length}</span>
            {/if}
            <div class="flex-1"></div>
            <IconButton label="Refresh" onclick={() => listTick++}>
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
                <path d="M21 12a9 9 0 1 1-2.6-6.4" />
                <path d="M21 3v6h-6" />
              </svg>
            </IconButton>
          </div>

          <div class="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
            {#if listError}
              <div class="px-2.5 py-2 text-[12px] text-red-400">{listError}</div>
            {/if}
            {#if listLoading}
              <div class="px-2.5 py-4 text-[12.5px] text-text-3">Loading inbox…</div>
            {:else if emails.length === 0 && !listError}
              <div class="px-2.5 py-6 text-center text-[12.5px] text-text-3">
                No messages came back.
                <div class="mt-1 text-[11.5px]">Refresh to check again.</div>
              </div>
            {:else}
              <div class="flex flex-col gap-0.5">
                {#each emails as m (m.id)}
                  <button
                    type="button"
                    onclick={() => void openEmail(m.id)}
                    class="w-full cursor-pointer rounded-[var(--r)] border-0 bg-transparent px-2.5 py-[9px] text-left {selectedId === m.id
                      ? 'bg-surface-hover'
                      : 'hover:bg-surface-hover'}"
                  >
                    <div class="flex items-baseline gap-2">
                      <span class="min-w-0 flex-1 truncate text-[12.5px] font-semibold text-text">
                        {m.from}
                      </span>
                      <span class="flex-none text-[11px] text-text-3">{fmtDate(m.date)}</span>
                    </div>
                    <div class="truncate text-[12.5px] text-text">{subjectOr(m.subject)}</div>
                    <div class="truncate text-[12px] text-text-3">{m.snippet}</div>
                  </button>
                {/each}
              </div>
            {/if}
          </div>

          <p class="flex-shrink-0 border-t border-border px-3 py-2 text-[10.5px] leading-[1.5] text-text-3">
            Gmail only today. Other providers later — the connection layer is
            per-account by design. Each profile connects its own inbox.
          </p>
        </div>

        <!-- reading pane -->
        <div class="min-w-0 overflow-y-auto">
          {#if selectedId == null}
            <div class="grid h-full place-items-center text-[12.5px] text-text-3">
              Select a message to read it
            </div>
          {:else if detailLoading}
            <div class="px-7 py-6 text-[12.5px] text-text-3">Loading message…</div>
          {:else if detailError}
            <div class="px-7 py-6 text-[12.5px] text-red-400">{detailError}</div>
          {:else if detail}
            <div class="mx-auto max-w-[720px] px-7 pb-10 pt-[26px]">
              <div class="text-[17px] font-semibold tracking-[-0.01em]">
                {subjectOr(detail.subject)}
              </div>
              <div class="mt-3 border-b border-border pb-3.5">
                <div class="text-[12.5px] font-semibold">{detail.from}</div>
                <div class="mt-0.5 text-[11.5px] text-text-3">
                  to {detail.to} · {fmtDate(detail.date)}
                </div>
              </div>
              <!-- Plain text only, escaped by Svelte — mail content is untrusted. -->
              <div class="whitespace-pre-wrap break-words pb-6 pt-4 text-[13.5px] leading-[1.6] text-text">
                {detail.body}
              </div>
            </div>
          {/if}
        </div>
      </div>
    {/if}

    <AppStatusBar />
  </main>
</div>

<!-- compose modal -->
{#if composeOpen}
  <div
    class="fixed inset-0 z-[80] grid place-items-center bg-black/45"
    role="presentation"
    onclick={(e) => {
      if (!sending && e.target === e.currentTarget) composeOpen = false;
    }}
    onkeydown={(e) => {
      if (!sending && e.key === "Escape") composeOpen = false;
    }}
  >
    <div
      class="w-[min(560px,94vw)] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow-pop)]"
      role="dialog"
      aria-modal="true"
      aria-label="Compose email"
    >
      <div class="flex items-center gap-2.5 border-b border-border px-4 py-[13px]">
        <span class="text-[13px] font-semibold">New message</span>
        <span class="min-w-0 truncate text-[11.5px] text-text-3">
          from {status?.account_email ?? "your Gmail account"}
        </span>
        <div class="flex-1"></div>
        <IconButton label="Close" disabled={sending} onclick={() => (composeOpen = false)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M6 6l12 12M18 6 6 18" />
          </svg>
        </IconButton>
      </div>

      {#if sentTo != null}
        <div class="p-4">
          <div
            class="flex items-center gap-2 rounded-[var(--r)] bg-local-soft px-3 py-2.5 text-[12.5px] font-[550] text-local"
          >
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" class="shrink-0">
              <path d="M4 12l5 5L20 7" />
            </svg>
            Sent to {sentTo}
          </div>
        </div>
        <div class="flex items-center border-t border-border px-4 py-3">
          <div class="flex-1"></div>
          <Button variant="primary" onclick={() => (composeOpen = false)}>Done</Button>
        </div>
      {:else}
        <div class="flex flex-col gap-2 p-4">
          <input
            bind:value={composeTo}
            placeholder="to@example.com"
            spellcheck="false"
            autocomplete="off"
            class={modalInput}
          />
          <input bind:value={composeSubject} placeholder="Subject" class={modalInput} />
          <textarea
            bind:value={composeBody}
            placeholder="Write your message…"
            rows="9"
            class="{modalInput} resize-y leading-[1.55]"
          ></textarea>
          {#if sendError}
            <div class="text-[12.5px] text-red-400">{sendError}</div>
          {/if}
        </div>
        <div class="flex items-center gap-2 border-t border-border px-4 py-3">
          <span class="text-[11.5px] text-text-3">sends immediately as you — no drafts</span>
          <div class="flex-1"></div>
          <Button variant="ghost" disabled={sending} onclick={() => (composeOpen = false)}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!canSend} onclick={() => void doSend()}>
            {sending ? "Sending…" : "Send"}
          </Button>
        </div>
      {/if}
    </div>
  </div>
{/if}
