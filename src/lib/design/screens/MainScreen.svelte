<script lang="ts">
  // Main Screen — the hub: sidebar, live thread, composer, and the right-hand
  // "why was this routed here" panel. Ported from MainScreen.tsx.
  // The bespoke binding pill and right-panel tab rail have no library component
  // (one-off template chrome) — reproduced with Tailwind + the `.lh-tab` /
  // `.lh-ghost-btn` prototype helpers for hover.
  import ChatMessage from "../components/ChatMessage.svelte";
  import IconButton from "../components/IconButton.svelte";
  import ModelPicker from "../components/ModelPicker.svelte";
  import PrivacyEventBar from "../components/PrivacyEventBar.svelte";
  import RoutingBadge from "../components/RoutingBadge.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import { nav } from "$lib/design/nav.svelte";
  import type { Binding, Route } from "$lib/design/types";
  import {
    activeConversation,
    streamingMessage,
    sendMessage as sendChatMessage,
    type Message,
  } from "$lib/stores/chat";
  import {
    providersStore,
    setActiveModel,
    getProvider,
    fetchModels,
  } from "$lib/stores/providers.svelte";
  import { sendOnEnter } from "$lib/stores/settings";
  import {
    explainClassification,
    type ClassificationExplanation,
    type ClassificationSpan,
  } from "$lib/api/tauri";

  type PanelTab = "routing" | "files" | "tasks" | "agents" | "terminal";
  type ModelOption = { name: string; kind: "local" | "cloud"; group: string; key: string };

  const BINDING_LABEL: Record<Binding, string> = {
    auto: "Auto",
    public: "Public",
    private: "Private",
  };
  const BINDING_DESC: Record<Binding, string> = {
    auto: "Routing decides per message; this chat is running locally",
    public: "Cloud models allowed for this conversation",
    private: "Nothing leaves this Mac",
  };
  const NEXT_BINDING: Record<Binding, Binding> = {
    auto: "public",
    public: "private",
    private: "auto",
  };

  const TABS: { id: PanelTab; label: string }[] = [
    { id: "routing", label: "Routing" },
    { id: "files", label: "Files in this chat" },
    { id: "tasks", label: "Background tasks" },
    { id: "agents", label: "Sub-agents" },
    { id: "terminal", label: "Terminal" },
  ];

  // This screen's per-send binding override (Auto/Public/Private) — feeds
  // sendMessage() directly (see chat.ts's bindingOverride param) rather than
  // mutating the conversation's stored default.
  let binding = $state<Binding>("auto");
  let whyOpen = $state(false);
  let panelTab = $state<PanelTab>("routing");

  // Composer draft. Send on click or (respecting sendOnEnter) on Enter.
  let draft = $state("");
  let isSending = $state(false);
  let textareaEl: HTMLTextAreaElement | null = $state(null);

  const cycleBinding = () => (binding = NEXT_BINDING[binding]);
  const toggleWhy = () => (whyOpen = !whyOpen);
  const openTab = (t: PanelTab) => {
    whyOpen = true;
    panelTab = t;
  };

  // ── "Why this was routed" — real classifier explanation of the last user
  // message (PLAN §11: censorship surfaced, never silent + the annotated view).
  let explanation = $state<ClassificationExplanation | null>(null);
  let explaining = $state(false);

  // The message the routing panel explains: the most recent user turn.
  const lastUserMessage = $derived(
    [...($activeConversation?.messages ?? [])]
      .reverse()
      .find((m) => m.role === "user")?.content ?? "",
  );

  // Load the explanation whenever the routing tab is open, re-loading if the
  // last user message changes under it. The classifier is the same one the §7
  // gate uses, so this matches the actual routing decision.
  $effect(() => {
    if (!whyOpen || panelTab !== "routing") return;
    const text = lastUserMessage;
    if (!text.trim()) {
      explanation = null;
      return;
    }
    explaining = true;
    explainClassification(text)
      .then((e) => {
        explanation = e;
      })
      .catch(() => {
        explanation = null;
      })
      .finally(() => {
        explaining = false;
      });
  });

  // Split the message into plain / marked segments at the classifier's span
  // offsets (Unicode scalar values → slice over a code-point array so multi-byte
  // characters don't shift the marks).
  type AnnotatedSegment = { text: string; kind: "plain" | "span" | "hard" };
  function annotateMessage(text: string, spans: ClassificationSpan[]): AnnotatedSegment[] {
    if (spans.length === 0) return [{ text, kind: "plain" }];
    const chars = Array.from(text);
    const sorted = [...spans].sort((a, b) => a.start - b.start);
    const segs: AnnotatedSegment[] = [];
    let cursor = 0;
    for (const s of sorted) {
      const start = Math.max(s.start, cursor);
      if (start >= s.end) continue; // fully overlapped by an earlier span
      if (start > cursor) segs.push({ text: chars.slice(cursor, start).join(""), kind: "plain" });
      segs.push({ text: chars.slice(start, s.end).join(""), kind: s.hard ? "hard" : "span" });
      cursor = s.end;
    }
    if (cursor < chars.length) segs.push({ text: chars.slice(cursor).join(""), kind: "plain" });
    return segs;
  }

  const VERDICT_HEADING: Record<ClassificationExplanation["label"], string> = {
    private: "Why this stayed local",
    uncertain: "Kept local — borderline",
    public: "Nothing sensitive detected",
  };

  // ── Model picker: build the design's flat ModelOption[] from the real
  // provider list, fetching each provider's models (cached by the store).
  let modelOptions = $state<ModelOption[]>([]);
  // Keyed by the composite `providerId::name` so two providers that expose an
  // identically-named model don't collide (the old name-keyed map let the
  // last-registered provider silently shadow the others).
  let modelOwner = new Map<string, { providerId: string; name: string }>();

  const modelKey = (providerId: string, name: string) => `${providerId}::${name}`;

  $effect(() => {
    const provs = providersStore.providers;
    let cancelled = false;
    (async () => {
      const perProvider = await Promise.all(
        provs.map(async (p) => ({ provider: p, models: await fetchModels(p.id) })),
      );
      if (cancelled) return;
      const opts: ModelOption[] = [];
      const owner = new Map<string, { providerId: string; name: string }>();
      for (const { provider, models } of perProvider) {
        for (const name of models) {
          const key = modelKey(provider.id, name);
          opts.push({
            name,
            kind: provider.kind === "cloud" ? "cloud" : "local",
            group: provider.name,
            key,
          });
          owner.set(key, { providerId: provider.id, name });
        }
      }
      modelOptions = opts;
      modelOwner = owner;
    })();
    return () => {
      cancelled = true;
    };
  });

  function handleModelChange(key: string) {
    const owner = modelOwner.get(key);
    if (owner) setActiveModel(owner.providerId, owner.name);
  }

  // The picker's selection identity: the active provider+model as a composite
  // key, or "" when nothing is selected (the picker then shows its placeholder).
  const activeModelKey = $derived(
    providersStore.activeProviderId && providersStore.activeModel
      ? modelKey(providersStore.activeProviderId, providersStore.activeModel)
      : "",
  );

  // ── Routing: map a real message's gate outcome to the design's Route.
  // "allow" means the gate let the request go to whichever endpoint was
  // targeted — local or cloud depends on that provider's kind, so allow
  // alone doesn't tell us the route; cross-reference providersStore.
  function messageRoute(m: Message): Route {
    if (m.error_source === "gate" || m.routing_decision === "block") return "blocked";
    if (m.routing_decision === "route_local") return "local";
    if (m.routing_decision === "allow") {
      const provider = getProvider(m.provider_id ?? null);
      if (provider) return provider.kind === "cloud" ? "cloud" : "local";
    }
    return "local";
  }

  function routeLabel(m: Message): string {
    const route = messageRoute(m);
    const prefix = route === "local" ? "Local" : route === "cloud" ? "Cloud" : "Held";
    return m.model ? `${prefix} · ${m.model}` : prefix;
  }

  // Only show a routing badge once a real decision exists — never on a
  // still-streaming turn, and never fabricated for a plain model/network
  // error (those aren't a privacy/routing outcome).
  function hasRoutingSignal(m: Message): boolean {
    return !m.streaming && (m.routing_decision != null || m.error_source === "gate");
  }

  // Split into paragraphs on blank lines; each <p> keeps internal newlines
  // via CSS (whitespace-pre-wrap) rather than injecting HTML.
  function paragraphs(content: string): string[] {
    const parts = content.split(/\n{2,}/).filter((p) => p.length > 0);
    return parts.length > 0 ? parts : [content];
  }

  // ── Composer ─────────────────────────────────────────────────────────────
  function autoresize() {
    if (!textareaEl) return;
    textareaEl.style.height = "auto";
    textareaEl.style.height = Math.min(textareaEl.scrollHeight, 150) + "px";
  }

  async function handleSend() {
    const content = draft.trim();
    if (!content || isSending) return;
    isSending = true;
    draft = "";
    autoresize();
    try {
      await sendChatMessage(
        content,
        providersStore.activeProviderId,
        providersStore.activeModel,
        binding,
      );
    } finally {
      isSending = false;
      textareaEl?.focus();
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key !== "Enter") return;
    if (!$sendOnEnter) return; // Enter inserts a newline; no explicit Send action here
    if (e.shiftKey) return; // Shift+Enter always inserts a newline
    e.preventDefault();
    handleSend();
  }

  // Reusable chrome recipes (mirror the React inline styles).
  const card =
    "px-3 py-[10px] bg-surface border border-border rounded-[var(--r-lg)]";
</script>

{#snippet dot(cls: string)}
  <span class="h-[7px] w-[7px] shrink-0 rounded-full {cls}"></span>
{/snippet}

{#snippet tabIcon(id: PanelTab)}
  {#if id === "routing"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
      <path d="M4 6h8M4 12h6M4 18h8" />
      <path d="M15 6h5M17 12h3M15 18h5" opacity=".5" />
    </svg>
  {:else if id === "files"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
      <path d="M3 8a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" />
    </svg>
  {:else if id === "tasks"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
      <path d="M12 7v5l3 2" />
      <circle cx="12" cy="12" r="8" />
    </svg>
  {:else if id === "agents"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
      <circle cx="6" cy="6" r="2.5" />
      <circle cx="18" cy="6" r="2.5" />
      <circle cx="12" cy="18" r="2.5" />
      <path d="M6 8.5v3a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2v-3M12 13.5v2" />
    </svg>
  {:else}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
      <rect x="3" y="4" width="18" height="16" rx="2" />
      <path d="M7 9l3 3-3 3M13 15h4" />
    </svg>
  {/if}
{/snippet}

<div
  class="grid h-screen transition-[grid-template-columns] duration-200 ease-out {whyOpen
    ? 'grid-cols-[260px_1fr_350px]'
    : 'grid-cols-[260px_1fr_0px]'}"
>
  <Sidebar activeConv={$activeConversation?.name ?? "New chat"} engineState="local" />

  <main class="flex min-h-0 min-w-0 flex-col">
    <div
      class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]"
    >
      <div class="min-w-0 truncate text-[13.5px] font-semibold">
        {$activeConversation?.name ?? "New chat"}
      </div>

      <button
        type="button"
        onclick={cycleBinding}
        title={BINDING_DESC[binding]}
        aria-label="Conversation binding — click to switch"
        class="inline-flex h-7 cursor-pointer items-center gap-[7px] rounded-[14px] border border-border-strong bg-surface px-3 text-[12px] font-semibold tracking-[0.02em] text-text"
      >
        {@render dot(binding === "public" ? "bg-cloud" : "bg-local")}
        {BINDING_LABEL[binding]}
      </button>

      <div class="flex-1"></div>

      <div class="flex flex-shrink-0 items-center gap-1">
        <button
          type="button"
          onclick={toggleWhy}
          title="Show routing details for this conversation"
          aria-label="Why was this routed here?"
          aria-pressed={whyOpen}
          class="inline-flex h-7 cursor-pointer items-center gap-1.5 rounded-[var(--r)] border border-border bg-transparent px-2.5 text-[12px] font-medium text-text-2"
        >
          <svg
            width="13"
            height="13"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            class="shrink-0"
          >
            <path d="M4 6h8M4 12h6M4 18h8" />
            <path d="M15 6h5M17 12h3M15 18h5" opacity=".45" />
          </svg>
          Routing
        </button>
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

    <div class="lh-messages flex-1 overflow-y-auto px-5 py-[26px]">
      <div class="mx-auto flex max-w-[700px] flex-col gap-5">
        {#each $activeConversation?.messages ?? [] as m (m.id)}
          {#if m.role === "user"}
            <ChatMessage role="user">
              <span class="whitespace-pre-wrap">{m.content}</span>
            </ChatMessage>
          {:else}
            {#if m.routing_decision === "route_local"}
              <PrivacyEventBar kind="kept" title="Kept on your machine">
                This turn was routed to a local model — the content looked
                sensitive, or a cloud endpoint wasn't reachable.
              </PrivacyEventBar>
            {:else if m.error_source === "gate"}
              <PrivacyEventBar kind="stop" title="Held from leaving this machine">
                {m.error ?? "The privacy gate held this message back."}
              </PrivacyEventBar>
            {/if}

            {#snippet msgBadge()}
              <RoutingBadge
                route={messageRoute(m)}
                label={routeLabel(m)}
                onclick={toggleWhy}
              />
            {/snippet}

            <ChatMessage role="assistant" badge={hasRoutingSignal(m) ? msgBadge : undefined}>
              {#if m.streaming && m.content.length === 0}
                <p class="lh-thinking m-0 flex items-center gap-[3px] text-text-3">
                  <span class="lh-dot"></span><span class="lh-dot"></span><span class="lh-dot"></span>
                </p>
              {:else}
                {#each paragraphs(m.content) as para}
                  <p class="whitespace-pre-wrap {m.error ? 'text-blocked' : ''}">{para}</p>
                {/each}
              {/if}
            </ChatMessage>
          {/if}
        {/each}
      </div>
    </div>

    <div class="flex-shrink-0 px-5 pb-4 pt-1">
      <div
        class="mx-auto max-w-[700px] rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow)] transition-colors duration-100 focus-within:border-[color-mix(in_srgb,var(--accent)_45%,var(--border-strong))]"
      >
        <div class="flex items-center gap-2 py-[7px] pl-3 pr-[7px]">
          <button
            type="button"
            aria-label="Attach"
            class="grid h-[30px] w-[30px] flex-shrink-0 place-items-center self-end rounded-[var(--r)] border-0 bg-transparent text-text-3 hover:bg-surface-hover hover:text-text-2"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M12 5v14M5 12h14" />
            </svg>
          </button>
          <textarea
            bind:this={textareaEl}
            bind:value={draft}
            rows="1"
            placeholder="Message Lost Harness…"
            onkeydown={handleKeydown}
            oninput={autoresize}
            class="max-h-[150px] min-w-0 flex-1 resize-none border-0 bg-transparent py-[5px] text-[14px] leading-[1.55] text-text outline-none placeholder:text-text-3"
          ></textarea>
          <span class="flex-shrink-0 whitespace-nowrap">
            <ModelPicker
              models={modelOptions}
              value={activeModelKey}
              onchange={handleModelChange}
            />
          </span>
          <button
            type="button"
            aria-label="Voice input"
            class="grid h-[30px] w-[30px] flex-shrink-0 place-items-center self-end rounded-[var(--r)] border-0 bg-transparent text-text-3 hover:bg-surface-hover hover:text-text-2"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <rect x="9" y="3" width="6" height="11" rx="3" />
              <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
            </svg>
          </button>
          <button
            type="button"
            aria-label="Send"
            onclick={handleSend}
            class="grid h-[30px] w-[30px] flex-shrink-0 place-items-center self-end rounded-[var(--r)] border-0 bg-accent text-on-accent transition duration-100 hover:brightness-[1.06]"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M12 19V5M5 12l7-7 7 7" />
            </svg>
          </button>
        </div>
      </div>
    </div>

    <AppStatusBar {binding} session="0:12" />
  </main>

  <div class="min-w-0 overflow-hidden">
    <aside
      class="flex h-full min-w-0 flex-col border-l border-border bg-sidebar"
    >
      <div
        class="flex h-12 flex-shrink-0 items-center gap-[3px] border-b border-border pl-[10px] pr-2"
      >
        {#each TABS as t (t.id)}
          <button
            type="button"
            aria-label={t.label}
            aria-pressed={panelTab === t.id}
            title={t.label}
            onclick={() => openTab(t.id)}
            class="grid h-[34px] w-[34px] flex-shrink-0 cursor-pointer place-items-center rounded-[7px] border-0 transition-[0.1s] {panelTab ===
            t.id
              ? 'bg-surface-2 text-text'
              : 'bg-transparent text-text-3 hover:bg-surface-hover hover:text-text'}"
          >
            {@render tabIcon(t.id)}
          </button>
        {/each}
        <div class="flex-1"></div>
        <button
          type="button"
          aria-label="Close panel"
          onclick={() => (whyOpen = false)}
          class="grid h-7 w-7 cursor-pointer place-items-center rounded-[6px] border-0 bg-transparent text-text-3 transition-[0.1s] hover:bg-surface-hover hover:text-text"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M6 6l12 12M18 6 6 18" />
          </svg>
        </button>
      </div>

      <div class="min-h-0 flex-1 overflow-y-auto">
        {#if panelTab === "routing"}
          <div class="px-[14px] py-[15px]">
            {#if explaining && !explanation}
              <p class="text-[12px] text-text-3">Checking this message…</p>
            {:else if !explanation}
              <p class="text-[12px] text-text-3">
                Send a message to see how the privacy guard classified it.
              </p>
            {:else}
              <div class="mb-2.5 flex items-center gap-2">
                <span class="text-[12.5px] font-semibold">
                  {VERDICT_HEADING[explanation.label]}
                </span>
              </div>
              <RoutingBadge
                route={explanation.label === "public" ? "cloud" : "local"}
                label={explanation.label === "public" ? "Eligible for cloud" : "Kept on your machine"}
              />

              <div
                class="px-0 pb-1.5 pt-[14px] text-[10.5px] font-semibold uppercase tracking-[0.06em] text-text-3"
              >
                Your message
              </div>
              <div
                class="lh-annotated rounded-[var(--r)] border border-border bg-surface px-[11px] py-[9px] text-[12.5px] leading-[1.7]"
              >
                {#each annotateMessage(lastUserMessage, explanation.spans) as seg, i (i)}
                  {#if seg.kind === "plain"}{seg.text}{:else}<mark
                      class="span {seg.kind === 'hard' ? 'hard' : ''}">{seg.text}</mark
                    >{/if}
                {/each}
              </div>

              {#if explanation.spans.length > 0}
                <div
                  class="px-0 pb-1.5 pt-[14px] text-[10.5px] font-semibold uppercase tracking-[0.06em] text-text-3"
                >
                  What tripped the guard
                </div>
                <div class="flex flex-col gap-1.5">
                  {#each explanation.spans as s, i (i)}
                    <div
                      class="flex items-center gap-[9px] rounded-[var(--r)] border border-border bg-surface px-2.5 py-2"
                    >
                      {@render dot(s.hard ? "bg-blocked" : "bg-warn")}
                      <span class="flex-1 text-[12px]">
                        {s.label}{#if s.hard}<span class="font-semibold text-blocked">
                            · hard-block</span
                          >{/if}
                      </span>
                      <span class="text-[10.5px] text-text-3">{s.layer}</span>
                    </div>
                  {/each}
                </div>
              {:else}
                <p class="mt-3 text-[12px] text-text-3">
                  {explanation.label === "public"
                    ? "Nothing sensitive detected — this message is eligible for a cloud model."
                    : "The model read this as sensitive but found no exact spans to highlight — the whole message stays local."}
                </p>
              {/if}
            {/if}
          </div>
        {:else if panelTab === "files"}
          <div class="p-3">
            <div
              class="px-0.5 pb-2 pt-1 text-[10.5px] font-semibold uppercase tracking-[0.06em] text-text-3"
            >
              Touched in this chat
            </div>
            <div class="flex flex-col gap-1">
              <a
                href="#/editor"
                onclick={(e) => {
                  e.preventDefault();
                  nav.go("editor");
                }}
                class="flex items-center gap-2.5 rounded-[var(--r)] px-2.5 py-[9px] no-underline hover:bg-surface-hover"
              >
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--text-3)" stroke-width="1.7" class="shrink-0">
                  <path d="M6 2h9l5 5v15H6z" />
                  <path d="M15 2v5h5" />
                </svg>
                <span class="min-w-0 flex-1">
                  <span class="block text-[12.5px] text-text">heater-reply.md</span>
                  <span class="block text-[11px] text-text-3">Created just now</span>
                </span>
                {@render dot("bg-local")}
              </a>
              <a
                href="#/editor"
                onclick={(e) => {
                  e.preventDefault();
                  nav.go("editor");
                }}
                class="flex items-center gap-2.5 rounded-[var(--r)] px-2.5 py-[9px] no-underline hover:bg-surface-hover"
              >
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--text-3)" stroke-width="1.7" class="shrink-0">
                  <path d="M6 2h9l5 5v15H6z" />
                  <path d="M15 2v5h5" />
                </svg>
                <span class="min-w-0 flex-1">
                  <span class="block text-[12.5px] text-text">lease.pdf</span>
                  <span class="block text-[11px] text-text-3">Read for context</span>
                </span>
                {@render dot("bg-local")}
              </a>
            </div>
            <div class="px-0.5 pt-3 text-[11px] text-text-3">
              Only files the assistant opened or wrote in this conversation
              appear here.
            </div>
          </div>
        {:else if panelTab === "tasks"}
          <div class="flex flex-col gap-2 p-3">
            <div class={card}>
              <div class="flex items-center gap-2">
                <span class="flex-1 text-[12.5px] font-[550]">Indexing workspace</span>
                <span class="text-[11px] text-text-2">62%</span>
              </div>
              <div
                class="mt-2 h-[5px] overflow-hidden rounded-[3px] bg-surface-2"
              >
                <div class="h-full w-[62%] bg-accent"></div>
              </div>
            </div>
            <div class="{card} flex items-center gap-[9px]">
              {@render dot("bg-local")}
              <span class="flex-1 text-[12.5px] font-[550]">Inbox triage</span>
              <span class="text-[11px] text-text-3">next in 38m</span>
            </div>
            <div class="{card} flex items-center gap-[9px]">
              {@render dot("bg-blocked")}
              <span class="flex-1 text-[12.5px] font-[550]">Weekly expense rollup</span>
              <span class="text-[11px] text-blocked">held</span>
            </div>
          </div>
        {:else if panelTab === "agents"}
          <div class="flex flex-col gap-2 p-3">
            <div class={card}>
              <div class="flex items-center gap-[9px]">
                {@render dot("bg-local")}
                <span class="flex-1 text-[12.5px] font-semibold">research</span>
                <span class="text-[11px] text-local">running</span>
              </div>
              <div class="mt-[5px] pl-4 text-[11px] text-text-3">
                3 tools · 12k tokens · local
              </div>
            </div>
            <div class={card}>
              <div class="flex items-center gap-[9px]">
                {@render dot("bg-cloud")}
                <span class="flex-1 text-[12.5px] font-semibold">summarizer</span>
                <span class="text-[11px] text-cloud">cloud · Opus</span>
              </div>
              <div class="mt-[5px] pl-4 text-[11px] text-text-3">
                waiting on research
              </div>
            </div>
            <div class={card}>
              <div class="flex items-center gap-[9px]">
                {@render dot("bg-text-3")}
                <span class="flex-1 text-[12.5px] font-semibold">code-reviewer</span>
                <span class="text-[11px] text-text-3">done</span>
              </div>
              <div class="mt-[5px] pl-4 text-[11px] text-text-3">
                finished in 4.1s
              </div>
            </div>
          </div>
        {:else if panelTab === "terminal"}
          <div class="p-3">
            <div
              class="rounded-[var(--r-lg)] border border-border bg-[#0d0d0f] px-[13px] py-3 font-mono text-[11.5px] leading-[1.7] text-[#c8c8cf]"
            >
              <div>
                <span class="text-[#6fa8dc]">tadashi</span>
                <span class="text-[#8f8f99]">~/workspace</span> $ lh run --local
                summarize
              </div>
              <div class="text-[#8f8f99]">▸ loaded Qwen3-14B on tadashi (mps)</div>
              <div class="text-[#3fa87d]">▸ guard: 2 spans kept local</div>
              <div class="text-[#8f8f99]">▸ wrote heater-reply.md</div>
              <div class="text-[#c8c8cf]">done in 1.24s</div>
              <div class="mt-[3px]">
                <span class="text-[#6fa8dc]">tadashi</span>
                <span class="text-[#8f8f99]">~/workspace</span> $
                <span
                  class="lh-cursor inline-block h-[14px] w-[7px] align-[-2px] bg-[#c8c8cf]"
                ></span>
              </div>
            </div>
            <div
              class="mt-2 flex items-center gap-1.5 rounded-[var(--r)] border border-border-strong bg-surface px-[11px] py-[7px] font-mono text-[11.5px] text-text-3"
            >
              <span class="text-text-2">$</span>
              <input
                placeholder="run a command…"
                class="min-w-0 flex-1 border-0 bg-transparent font-mono text-text outline-none"
              />
            </div>
          </div>
        {/if}
      </div>
    </aside>
  </div>
</div>

<style>
  /* Irreducible: the message scroller's custom scrollbar (design's
     `.messages::-webkit-scrollbar` knobs) — utilities can't express these. */
  .lh-messages::-webkit-scrollbar {
    width: 10px;
  }
  .lh-messages::-webkit-scrollbar-thumb {
    background: var(--border-strong);
    border-radius: 999px;
    border: 3px solid transparent;
    background-clip: padding-box;
  }

  /* The terminal cursor blink (design's `lhblink` keyframes). */
  .lh-cursor {
    animation: lhblink 1.1s steps(1) infinite;
  }
  @keyframes lhblink {
    50% {
      opacity: 0;
    }
  }

  /* Streaming "thinking" indicator on an in-progress assistant turn with no
     tokens yet — three pulsing dots, no library component for this. */
  .lh-dot {
    display: inline-block;
    width: 4px;
    height: 4px;
    border-radius: 999px;
    background: currentColor;
    opacity: 0.35;
    animation: lhdotpulse 1.2s infinite ease-in-out;
  }
  .lh-dot:nth-child(2) {
    animation-delay: 0.15s;
  }
  .lh-dot:nth-child(3) {
    animation-delay: 0.3s;
  }
  @keyframes lhdotpulse {
    0%,
    80%,
    100% {
      opacity: 0.3;
      transform: translateY(0);
    }
    40% {
      opacity: 1;
      transform: translateY(-2px);
    }
  }

  /* Annotated message in the routing panel — detected spans are wrapped in
     <mark class="span"> / <mark class="span hard"> (mirrors WhyPanel + the
     design's `.span` / `.span.hard`). */
  .lh-annotated {
    white-space: pre-wrap;
    word-break: break-word;
  }
  .lh-annotated :global(.span) {
    border-radius: 3px;
    padding: 0 2px;
    background: var(--warn-soft);
    border-bottom: 1.5px dashed var(--warn);
  }
  .lh-annotated :global(.span.hard) {
    background: var(--blocked-soft);
    border-bottom-color: var(--blocked);
  }
</style>
