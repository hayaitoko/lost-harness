<script lang="ts">
  // Email — triaged inbox + reading pane with a locally-drafted reply whose send
  // is gated behind an explicit approval (send_email is `dangerous`, so "Allow
  // once" is deliberately NOT the primary action). Add-account setup modal.
  // Ported from ui/app/screens/Email.tsx.
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import RoutingBadge from "../components/RoutingBadge.svelte";
  import RiskBadge from "../components/RiskBadge.svelte";
  import Toggle from "../components/Toggle.svelte";
  import BindingControl from "../components/BindingControl.svelte";
  import { nav } from "$lib/design/nav.svelte";
  import type { Binding } from "$lib/design/types";

  const INBOXES = [
    { name: "All inboxes", sub: "2 accounts", meta: "3 unread across accounts" },
    { name: "alex@fastmail.com", sub: "Personal", meta: "alex@fastmail.com · 2 unread" },
    { name: "alex@parsons.dev", sub: "Work", meta: "alex@parsons.dev · 1 unread" },
  ];

  type Mail = {
    from: string;
    time: string;
    subject: string;
    preview: string;
    unread?: boolean;
    active?: boolean;
  };
  const TODAY: Mail[] = [
    { from: "Marcus Webb", time: "9:41", subject: "Re: Heater repair — Unit 4B", preview: "I can have someone out Thursday between 9 and 12…", active: true },
    { from: "Dr. Chen", time: "8:15", subject: "Your results are in", preview: "Everything came back within range except…", unread: true },
    { from: "Nina Alvarez", time: "7:52", subject: "Kyoto board — day trips", preview: "Added a column for Nara — take a look when…", unread: true },
  ];
  const EARLIER: Mail[] = [
    { from: "Ryokan Sato", time: "Sun", subject: "Booking confirmation — Oct 12–16", preview: "We look forward to welcoming you. Check-in…" },
    { from: "Fastmail", time: "Jun 30", subject: "Receipt for your subscription", preview: "Thanks for your payment of $5.00…" },
  ];

  const PROVIDERS = ["Fastmail", "Gmail", "IMAP"] as const;
  type Provider = (typeof PROVIDERS)[number];

  let inbox = $state("alex@fastmail.com");
  let inboxMenuOpen = $state(false);
  let setupOpen = $state(false);
  let step = $state(1);
  let provider = $state<Provider>("Fastmail");
  let permDraft = $state(true);
  let mailBinding = $state<Binding>("private");
  let triageOn = $state(true);
  let approvalOpen = $state(false);
  let toastVisible = $state(false);
  let setupToast = $state(false);

  let sendTimer: ReturnType<typeof setTimeout> | undefined;
  let setupTimer: ReturnType<typeof setTimeout> | undefined;

  $effect(() => () => {
    clearTimeout(sendTimer);
    clearTimeout(setupTimer);
  });

  const current = $derived(INBOXES.find((i) => i.name === inbox) ?? INBOXES[1]);
  const inboxLabel = $derived(current.name === "All inboxes" ? "All inboxes" : "Inbox");

  function allowOnce() {
    approvalOpen = false;
    toastVisible = true;
    clearTimeout(sendTimer);
    sendTimer = setTimeout(() => (toastVisible = false), 3200);
  }
  function finishSetup() {
    setupOpen = false;
    setupToast = true;
    clearTimeout(setupTimer);
    setupTimer = setTimeout(() => (setupToast = false), 3200);
  }

  const settingRow =
    "flex items-center justify-between gap-2.5 rounded-[var(--r)] bg-surface-2 px-3 py-2.5";
  const modalInput =
    "w-full rounded-[var(--r)] border border-border bg-surface-2 px-[11px] py-[9px] text-[13px] text-text outline-none placeholder:text-text-3";
</script>

<div class="grid h-screen grid-cols-[260px_1fr]">
  <Sidebar active="email" />

  <main class="flex min-h-0 min-w-0 flex-col">
    <!-- topbar -->
    <div class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]">
      <div class="relative">
        <button
          type="button"
          aria-haspopup="true"
          onclick={() => (inboxMenuOpen = !inboxMenuOpen)}
          class="flex cursor-pointer items-center gap-[7px] rounded-[6px] border-0 bg-transparent px-[7px] py-1 text-[13.5px] font-semibold text-text"
        >
          {inboxLabel}
          <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="text-text-3">
            <path d="M6 9l6 6 6-6" />
          </svg>
        </button>
        {#if inboxMenuOpen}
          <div
            class="absolute left-0 top-[34px] z-[45] w-[250px] rounded-[var(--r-lg)] border border-border-strong bg-surface p-[5px] shadow-[var(--shadow-pop)]"
          >
            {#each INBOXES as ib (ib.name)}
              <button
                type="button"
                onclick={() => {
                  inbox = ib.name;
                  inboxMenuOpen = false;
                }}
                class="flex w-full cursor-pointer items-center justify-between gap-2 rounded-[6px] border-0 bg-transparent px-[9px] py-[7px] text-left text-text hover:bg-surface-hover"
              >
                <span class="flex min-w-0 flex-col items-start">
                  <span class="text-[12.5px] font-[550]">{ib.name}</span>
                  <span class="text-[11px] text-text-3">{ib.sub}</span>
                </span>
                {#if ib.name === inbox}
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" class="flex-none text-accent">
                    <path d="M4 12l5 5L20 7" />
                  </svg>
                {/if}
              </button>
            {/each}
            <div class="mx-1 my-[5px] h-px bg-border"></div>
            <button
              type="button"
              onclick={() => {
                inboxMenuOpen = false;
                step = 1;
                setupOpen = true;
              }}
              class="flex w-full cursor-pointer items-center gap-2 rounded-[6px] border-0 bg-transparent px-[9px] py-2 text-left text-[12.5px] font-medium text-text-2 hover:bg-surface-hover"
            >
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M12 5v14M5 12h14" />
              </svg>
              Add account…
            </button>
          </div>
        {/if}
      </div>
      <span class="text-[11.5px] text-text-3">{current.meta}</span>
      <div class="flex-1"></div>
      <div class="flex flex-shrink-0 items-center gap-1">
        <Button variant="primary">Compose</Button>
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

    <!-- two-pane body: mail list + reading pane -->
    <div class="grid min-h-0 flex-1 grid-cols-[320px_minmax(0,1fr)]">
      <!-- list -->
      <div class="overflow-y-auto border-r border-border px-2 pb-3 pt-1.5">
        <div class="px-2.5 pb-1 pt-2.5 text-[10px] font-semibold uppercase tracking-[0.06em] text-text-3">
          Today
        </div>
        <div class="flex flex-col gap-0.5">
          {#each TODAY as m (m.from)}
            {@render mailItem(m)}
          {/each}
        </div>
        <div class="px-2.5 pb-1 pt-3 text-[10px] font-semibold uppercase tracking-[0.06em] text-text-3">
          Earlier
        </div>
        <div class="flex flex-col gap-0.5">
          {#each EARLIER as m (m.from)}
            {@render mailItem(m)}
          {/each}
        </div>
      </div>

      <!-- reading pane -->
      <div class="min-w-0 overflow-y-auto">
        <div class="mx-auto max-w-[720px] px-7 pb-10 pt-[26px]">
          <div class="text-[17px] font-semibold tracking-[-0.01em]">Re: Heater repair — Unit 4B</div>
          <div class="mt-3 flex items-center gap-2.5 border-b border-border pb-3.5">
            <div class="grid h-8 w-8 flex-none place-items-center rounded-full border border-border bg-surface-2 text-[12px] font-semibold text-text-2">
              MW
            </div>
            <div class="min-w-0 flex-1">
              <div class="text-[12.5px] font-semibold">
                Marcus Webb <span class="font-normal text-text-3">&lt;mwebb.properties@gmail.com&gt;</span>
              </div>
              <div class="text-[11.5px] text-text-3">to me · today 9:41</div>
            </div>
            <IconButton label="Reply">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
                <path d="M9 14 4 9l5-5" />
                <path d="M4 9h9a7 7 0 0 1 7 7v4" />
              </svg>
            </IconButton>
            <IconButton label="More">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
                <circle cx="5" cy="12" r="1.6" />
                <circle cx="12" cy="12" r="1.6" />
                <circle cx="19" cy="12" r="1.6" />
              </svg>
            </IconButton>
          </div>

          <div class="pb-5 pt-4 text-[13.5px]">
            <p class="mb-3">Hi Alex,</p>
            <p class="mb-3">
              Thanks for the note — sorry about the heater. I can have someone out
              <b>Thursday between 9 and 12</b>. If that doesn't work, the earliest after that is Monday.
            </p>
            <p class="mb-3">Let me know and I'll confirm with the technician.</p>
            <p class="m-0">Marcus</p>
          </div>

          <!-- drafted reply card -->
          <div class="overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow)]">
            <div class="flex items-center gap-2 border-b border-border px-3.5 py-2.5">
              <span class="text-[12px] font-semibold">Drafted reply</span>
              <RoutingBadge route="local" label="Local · Qwen3-14B" />
              <div class="flex-1"></div>
              <span class="text-[11px] text-text-3">nothing sent yet</span>
            </div>
            <div class="p-3.5 text-[13px]">
              <p class="mb-2.5">Hi Marcus,</p>
              <p class="mb-2.5">
                Thursday 9–12 works — I'll be home. Please confirm with the technician, and thanks for the quick turnaround.
              </p>
              <p class="m-0">Alex</p>
            </div>
            <div class="flex items-center gap-2 border-t border-border px-3.5 py-2.5">
              <Button variant="primary" onclick={() => (approvalOpen = true)}>Approve &amp; send</Button>
              <Button>Edit draft</Button>
              <Button variant="ghost">Discard</Button>
              <div class="flex-1"></div>
              <span class="text-[11px] text-text-3">send_email is gated — you approve every send</span>
            </div>
          </div>
        </div>
      </div>
    </div>

    <AppStatusBar session="0:12" />
  </main>
</div>

<!-- tool-approval overlay -->
{#if approvalOpen}
  <div class="fixed inset-0 z-50 grid place-items-center bg-black/45">
    <div
      class="w-[min(440px,94vw)] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-bg shadow-[var(--shadow-pop)]"
      role="dialog"
      aria-modal="true"
      aria-label="Tool approval"
    >
      <div class="border-b border-border px-4 pb-3 pt-3.5">
        <div class="mb-1 flex items-center gap-[9px]">
          <div class="grid h-[30px] w-[30px] place-items-center rounded-[var(--r)] bg-surface-2 text-text-2">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9">
              <path d="M12 3l7 4v5c0 4-3 7-7 9-4-2-7-5-7-9V7l7-4Z" />
            </svg>
          </div>
          <div class="flex-1 text-[14px] font-semibold">
            Run <code class="rounded-[var(--r-sm)] bg-surface-2 px-[5px] py-[1.5px] font-mono text-[12.5px]">send_email</code>?
          </div>
          <RiskBadge risk="dangerous" />
        </div>
        <div class="text-[12px] text-text-3">Network · leaves this machine</div>
      </div>
      <div class="px-4 py-3.5">
        <div class="mb-1.5 text-[10.5px] font-[650] uppercase tracking-[0.05em] text-text-3">It will run</div>
        <div class="whitespace-pre-wrap break-words rounded-[var(--r)] border border-border bg-surface px-3 py-2.5 font-mono text-[12px] leading-[1.5] text-text">
          send_email(<br />
          &nbsp;&nbsp;to: <span class="text-cloud">"mwebb.properties@gmail.com"</span>,<br />
          &nbsp;&nbsp;subject: <span class="text-cloud">"Re: Heater repair — Unit 4B"</span><br />)
        </div>
      </div>
      <div class="flex items-center gap-2 border-t border-border px-4 py-3">
        <span class="text-[11.5px] text-text-3">last in queue</span>
        <div class="ml-auto flex gap-2">
          <Button variant="ghost" onclick={() => (approvalOpen = false)}>Deny</Button>
          <!-- dangerous risk: Allow once is deliberately NOT primary -->
          <Button onclick={allowOnce}>Allow once</Button>
          <Button onclick={() => (approvalOpen = false)}>Allow session</Button>
        </div>
      </div>
    </div>
  </div>
{/if}

<!-- toasts -->
{#if toastVisible}
  {@render toast("Approved once — reply sent to Marcus")}
{/if}
{#if setupToast}
  {@render toast("Account added — first triage runs in the background")}
{/if}

<!-- add-account setup modal -->
{#if setupOpen}
  <div class="fixed inset-0 z-[80] grid place-items-center bg-black/45 backdrop-blur-[3px]">
    <div class="w-[500px] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow-pop)]">
      <div class="flex items-center gap-2.5 border-b border-border px-4 py-[13px]">
        <span class="text-[13px] font-semibold">Add an email account</span>
        <span class="text-[11.5px] text-text-3">Step {step} of 3</span>
        <div class="flex-1"></div>
        <IconButton label="Close" onclick={() => (setupOpen = false)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M6 6l12 12M18 6 6 18" />
          </svg>
        </IconButton>
      </div>

      <div class="min-h-[196px] p-4">
        {#if step === 1}
          <div class="mb-1.5 text-[12.5px] font-semibold">Choose a provider</div>
          <div class="mb-3 text-[12px] text-text-3">
            Credentials are stored in the macOS keychain — never in Lost Harness's own files.
          </div>
          <div class="grid grid-cols-3 gap-2">
            {#each PROVIDERS as p (p)}
              <button
                type="button"
                onclick={() => (provider = p)}
                class="cursor-pointer rounded-[var(--r)] border px-2 py-3 text-[12.5px] font-semibold transition
                  {provider === p
                  ? 'border-border-strong bg-surface-2 text-text'
                  : 'border-border bg-transparent text-text-2'}"
              >
                {p}
              </button>
            {/each}
          </div>
          <input placeholder="you@example.com" class="{modalInput} mt-3" />
        {:else if step === 2}
          <div class="mb-1.5 text-[12.5px] font-semibold">What may the assistant do?</div>
          <div class="mb-3 text-[12px] text-text-3">
            Reading and drafting happen locally. Sending is always gated behind your approval.
          </div>
          <div class="flex flex-col gap-2">
            <div class={settingRow}>
              <span class="text-[12.5px]">Read mail locally</span>
              <Toggle checked locked label="Read mail locally" />
            </div>
            <div class={settingRow}>
              <span class="text-[12.5px]">Draft replies automatically</span>
              <Toggle checked={permDraft} onchange={(v) => (permDraft = v)} label="Draft replies automatically" />
            </div>
            <div class={settingRow}>
              <span class="text-[12.5px]">
                Send without approval <span class="text-text-3">— never available</span>
              </span>
              <Toggle checked={false} locked label="Send without approval" />
            </div>
          </div>
        {:else}
          <div class="mb-1.5 text-[12.5px] font-semibold">Routing for this account</div>
          <div class="mb-3 text-[12px] text-text-3">
            Mail content follows this binding when the assistant works with it.
          </div>
          <BindingControl value={mailBinding} onchange={(b) => (mailBinding = b)} />
          <div class="{settingRow} mt-3.5">
            <span class="text-[12.5px]">Triage new mail on a schedule</span>
            <Toggle checked={triageOn} onchange={(v) => (triageOn = v)} label="Triage new mail on a schedule" />
          </div>
        {/if}
      </div>

      <div class="flex items-center gap-2 border-t border-border px-4 py-3">
        {#if step > 1}
          <Button variant="ghost" onclick={() => (step = Math.max(1, step - 1))}>Back</Button>
        {/if}
        <div class="flex-1"></div>
        {#if step < 3}
          <Button variant="primary" onclick={() => (step = Math.min(3, step + 1))}>Next</Button>
        {:else}
          <Button variant="primary" onclick={finishSetup}>Add account</Button>
        {/if}
      </div>
    </div>
  </div>
{/if}

<!-- A single triaged mail row (`.mail-item`). Decorative in this sample — no handler. -->
{#snippet mailItem(m: Mail)}
  <div class="cursor-pointer rounded-[var(--r)] px-2.5 py-[9px] {m.active ? 'bg-surface-hover' : ''}">
    <div class="flex items-baseline gap-2">
      {#if m.unread}
        <span class="h-[7px] w-[7px] flex-none self-center rounded-full bg-accent"></span>
      {/if}
      <span class="min-w-0 flex-1 truncate text-[12.5px] font-semibold">{m.from}</span>
      <span class="flex-none text-[11px] text-text-3">{m.time}</span>
    </div>
    <div class="truncate text-[12.5px] {m.unread ? 'font-semibold' : 'font-normal'}">{m.subject}</div>
    <div class="truncate text-[12px] text-text-3">{m.preview}</div>
  </div>
{/snippet}

<!-- Transient bottom-center notification (`.toast.show`). -->
{#snippet toast(message: string)}
  <div class="fixed bottom-11 left-1/2 z-[60] -translate-x-1/2">
    <div class="flex max-w-[340px] items-center gap-2 rounded-[var(--r)] border border-border-strong bg-surface px-3.5 py-[9px] text-[12.5px] text-text shadow-[var(--shadow-pop)]">
      <span class="h-1.5 w-1.5 flex-none rounded-full bg-accent"></span>
      <span>{message}</span>
    </div>
  </div>
{/snippet}
