<script lang="ts">
  // Main Screen — the hub: sidebar, live thread, composer, and the right-hand
  // "why was this routed here" panel. Ported from MainScreen.tsx.
  // The bespoke binding pill and right-panel tab rail have no library component
  // (one-off template chrome) — reproduced with Tailwind + the `.lh-tab` /
  // `.lh-ghost-btn` prototype helpers for hover.
  import ChatMessage from "../components/ChatMessage.svelte";
  import IconButton from "../components/IconButton.svelte";
  import ModelPicker, {
    type ModelGroup,
    type ArmedSelection,
  } from "../components/ModelPicker.svelte";
  import PrivacyEventBar from "../components/PrivacyEventBar.svelte";
  import RoutingBadge from "../components/RoutingBadge.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import type { KnotState } from "../components/Knot.svelte";
  import { nav } from "$lib/design/nav.svelte";
  import type { Binding, Route } from "$lib/design/types";
  import {
    activeConversation,
    activeConversationId,
    streamingMessage,
    sendMessage as sendChatMessage,
    cancelActiveStream,
    setConversationBinding as persistConversationBinding,
    type Message,
  } from "$lib/stores/chat";
  import {
    providersStore,
    setActiveModel,
    fetchModels,
    type Provider,
  } from "$lib/stores/providers.svelte";
  import { sendOnEnter } from "$lib/stores/settings";
  import { activeProfileId } from "$lib/stores/profiles";
  import {
    explainClassification,
    confirmPublicSend,
    onMemoryEvent,
    listWorkspaceFiles,
    type ClassificationExplanation,
    type ClassificationSpan,
    type WorkspaceEntry,
  } from "$lib/api/tauri";

  // Nav honesty: only tabs with real backing data. The mock tasks/agents/
  // terminal tabs were removed with the other fiction surfaces (2026-07-24).
  type PanelTab = "routing" | "files";

  /** Shown when the composer has no endpoint armed. Mirrors the backend's
   *  `NO_ENDPOINT_SELECTED` (agent/loop_mod.rs) so the user reads one
   *  sentence no matter which layer catches it. */
  const NO_ENDPOINT_SELECTED =
    "no model endpoint is selected — pick a model in the composer";

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

  // Q11 permission mode for this chat. Normal is the default; Plan is
  // read-only (the agent can look but not change); Accept-edits auto-approves
  // local edits (never off-box / dangerous actions). Feeds sendMessage().
  type SessionMode = "normal" | "plan" | "accept_edits";
  const MODE_LABEL: Record<SessionMode, string> = {
    normal: "Ask",
    plan: "Plan",
    accept_edits: "Bypass local edits",
  };
  const MODE_DESC: Record<SessionMode, string> = {
    normal: "Tool actions ask for approval as usual",
    plan: "Read-only — the agent can look and plan but makes no changes",
    accept_edits: "Auto-approve local edits (never off-box or dangerous actions)",
  };
  const MODE_OPTIONS: { id: SessionMode; label: string; description: string }[] = [
    { id: "normal", label: MODE_LABEL.normal, description: MODE_DESC.normal },
    { id: "plan", label: MODE_LABEL.plan, description: MODE_DESC.plan },
    { id: "accept_edits", label: MODE_LABEL.accept_edits, description: MODE_DESC.accept_edits },
  ];

  const TABS: { id: PanelTab; label: string }[] = [
    { id: "routing", label: "Routing" },
    { id: "files", label: "Workspace files" },
  ];

  // This is the conversation's persisted routing intent. Before the first
  // send it is held locally and becomes the new conversation's binding.
  let binding = $state<Binding>("auto");
  let bindingSaving = $state(false);
  let bindingError = $state<string | null>(null);
  let mode = $state<SessionMode>("normal");
  let permissionOpen = $state(false);
  let permissionEl: HTMLDivElement | null = $state(null);
  let whyOpen = $state(false);
  let panelTab = $state<PanelTab>("routing");

  // Composer draft. Send on click or (respecting sendOnEnter) on Enter.
  let draft = $state("");
  let isSending = $state(false);
  let textareaEl: HTMLTextAreaElement | null = $state(null);
  type ComposerPopover = "attachments" | "context" | null;
  let composerPopover = $state<ComposerPopover>(null);
  let attachmentEl: HTMLDivElement | null = $state(null);
  let contextEl: HTMLDivElement | null = $state(null);
  let listening = $state(false);
  let voiceNotice = $state<string | null>(null);

  $effect(() => {
    if ($activeConversation) binding = $activeConversation.binding;
  });

  async function cycleBinding() {
    if (bindingSaving) return;
    const next = NEXT_BINDING[binding];
    const conversation = $activeConversation;
    if (!conversation) {
      binding = next;
      return;
    }
    bindingSaving = true;
    bindingError = null;
    try {
      await persistConversationBinding(conversation.id, next);
      binding = next;
    } catch (err) {
      bindingError = `Couldn't save routing preference: ${String(err)}`;
    } finally {
      bindingSaving = false;
    }
  }
  const selectMode = (next: SessionMode) => {
    mode = next;
    permissionOpen = false;
  };
  const toggleWhy = () => (whyOpen = !whyOpen);
  const openTab = (t: PanelTab) => {
    whyOpen = true;
    panelTab = t;
  };

  $effect(() => {
    if (!permissionOpen && !composerPopover) return;
    const onDocMouseDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (
        permissionEl?.contains(target) ||
        attachmentEl?.contains(target) ||
        contextEl?.contains(target)
      ) {
        return;
      }
      permissionOpen = false;
      composerPopover = null;
    };
    document.addEventListener("mousedown", onDocMouseDown);
    return () => document.removeEventListener("mousedown", onDocMouseDown);
  });

  // ── Non-silent memory signal (PLAN §9): a transient "recalled N notes" (or
  // "remembered N notes") line when the agent injects or saves memory for the
  // current turn. A manual save from Settings emits with an empty
  // conversation_id, so it's filtered out here by design — the Settings pane
  // reloads its own list instead.
  let memoryNote = $state<string | null>(null);
  let memoryNoteTimer: ReturnType<typeof setTimeout> | null = null;
  $effect(() => {
    const un = onMemoryEvent((e) => {
      if (e.count < 1) return;
      if (e.kind !== "recalled" && e.kind !== "remembered") return;
      // Only surface the banner for the conversation being viewed.
      if (e.conversation_id !== $activeConversation?.id) return;
      memoryNote =
        e.kind === "recalled"
          ? `Recalled ${e.count} saved note${e.count === 1 ? "" : "s"} for this answer`
          : `Remembered ${e.count === 1 ? "a note" : `${e.count} notes`}`;
      if (memoryNoteTimer) clearTimeout(memoryNoteTimer);
      memoryNoteTimer = setTimeout(() => (memoryNote = null), 6000);
    });
    return () => {
      un.then((f) => f());
      if (memoryNoteTimer) clearTimeout(memoryNoteTimer);
    };
  });

  // ── Workspace files tab: the profile workspace's top level (read-only).
  // `wsSeq` drops stale responses (e.g. a slow listing landing after a
  // profile switch) so only the latest request may write state.
  let wsEntries = $state<WorkspaceEntry[]>([]);
  let wsError: string | null = $state(null);
  let wsLoading = $state(true);
  let wsSeq = 0;
  $effect(() => {
    if (!whyOpen || panelTab !== "files") return;
    const profile = $activeProfileId;
    const token = ++wsSeq;
    wsLoading = true;
    wsError = null;
    listWorkspaceFiles(profile, "")
      .then((rows) => {
        if (token === wsSeq) wsEntries = rows;
      })
      .catch((err) => {
        if (token === wsSeq) wsError = String(err);
      })
      .finally(() => {
        if (token === wsSeq) wsLoading = false;
      });
  });

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
    explainClassification(text, $activeProfileId)
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

  // ── Model picker: one group per configured provider, built from the real
  // provider list. A provider whose listing FAILED still gets a group, carrying
  // the reason — never dropped, because a provider missing from the popover is
  // what leaves a different one armed and serving every turn.
  let modelGroups = $state<ModelGroup[]>([]);
  // Keyed by the composite `providerId::name` so two providers that expose an
  // identically-named model don't collide (the old name-keyed map let the
  // last-registered provider silently shadow the others).
  let modelOwner = new Map<string, { providerId: string; name: string }>();

  const modelKey = (providerId: string, name: string) => `${providerId}::${name}`;

  // `modelSeq` drops a slow listing that lands after a newer one, so the
  // groups can never be rebuilt from a stale round.
  let modelSeq = 0;
  async function loadModelGroups(providers: Provider[], refresh: boolean) {
    const token = ++modelSeq;
    const perProvider = await Promise.all(
      providers.map(async (p) => ({
        provider: p,
        result: await fetchModels(p.id, { refresh }),
      })),
    );
    if (token !== modelSeq) return;
    const groups: ModelGroup[] = [];
    const owner = new Map<string, { providerId: string; name: string }>();
    for (const { provider, result } of perProvider) {
      // De-duplicate within one endpoint's listing: `key` is
      // `providerId::name`, so an endpoint that returns the same model name
      // twice would produce two identical keys and throw `each_key_duplicate`
      // in the (always-mounted) popover — the same crash class as a duplicate
      // group key, one level down.
      const models = result.ok ? [...new Set(result.models)] : [];
      for (const name of models) {
        owner.set(modelKey(provider.id, name), { providerId: provider.id, name });
      }
      groups.push({
        // Identity is the provider ID. Two providers may share a display
        // name (nothing enforces uniqueness — see ModelGroup.id), and a
        // name-keyed list would crash the whole composer when they do.
        id: provider.id,
        group: provider.name,
        // Same predicate the backend stamps turns with: `isPrivate` (the
        // base URL), NOT `kind` (a user-typed label with no enforcement
        // power). `kind === "cloud" ? cloud : local` painted a Custom
        // provider at https://api.example.com green and labelled it "on
        // device" — a trust-zone claim about an endpoint that egresses, made
        // at exactly the moment the user is choosing where to send.
        kind: provider.isPrivate ? "local" : "cloud",
        items: models.map((name) => ({ name, key: modelKey(provider.id, name) })),
        // Every empty group says why it is empty — a failed listing and an
        // endpoint with no models are different problems with different fixes.
        //
        // The "listed nothing" wording is deliberately blunt about the dead
        // end. Removing the Anthropic quick-add preset only helps NEW installs;
        // an existing `global.db` still has that row, and its user sees an
        // endpoint that is configured, answers, and can never be selected —
        // because this app speaks only the OpenAI-compatible surface and
        // Anthropic's native API rejects a Bearer key. We do not touch their
        // data, so the notice has to be enough to act on.
        notice: result.ok
          ? models.length === 0
            ? "This endpoint answered but listed no models, so nothing here can be selected. Lost Harness talks to OpenAI-compatible endpoints only — GET /models and POST /chat/completions with an Authorization: Bearer key. Check the base URL (it usually ends in /v1); if the service doesn't offer that API, remove it in Settings → Models."
            : null
          : `Couldn't list models — check the endpoint or key. (${result.error})`,
      });
    }
    modelGroups = groups;
    modelOwner = owner;
  }

  // Listing on provider-list changes only — a cache miss (new provider, edited
  // base URL) is the one thing that legitimately needs a live `GET /models`
  // without the user asking. Opening the picker no longer triggers anything.
  $effect(() => {
    void loadModelGroups(providersStore.providers, false);
  });

  /**
   * The picker's explicit "Refresh" affordance.
   *
   * This is now the ONLY interaction in the composer that contacts configured
   * endpoints on demand. Opening the picker used to run this on every click —
   * on open AND on close — bypassing the cache and issuing an authenticated
   * `GET {base_url}/models` to every provider, cloud ones included. In a
   * privacy-first app that is egress the user never asked for, so it is now
   * behind a button that says what it does.
   */
  let modelsRefreshing = $state(false);
  async function refreshModelGroups(): Promise<void> {
    if (modelsRefreshing) return;
    modelsRefreshing = true;
    try {
      await loadModelGroups(providersStore.providers, true);
    } finally {
      modelsRefreshing = false;
    }
  }

  function handleModelChange(key: string) {
    const owner = modelOwner.get(key);
    if (!owner) {
      composerError = "That model is no longer listed — pick another.";
      return;
    }
    // setActiveModel reports whether the selection actually took. Believing a
    // silent no-op would leave the PREVIOUS endpoint armed while the picker
    // appeared to have moved.
    if (!setActiveModel(owner.providerId, owner.name)) {
      composerError = "That endpoint is no longer configured — pick another model.";
      return;
    }
    composerError = null;
  }

  const activeProvider = $derived(
    providersStore.providers.find((provider) => provider.id === providersStore.activeProviderId),
  );

  /**
   * THE armed endpoint — the single expression every "is the composer armed?"
   * question in this screen answers from: the picker chip, the Send button's
   * enabled state and colour, and the knot.
   *
   * `handleSend` reads `providersStore.active` directly (it must — it is the
   * authority), and this is that same pair plus the provider row it names.
   * Anything that wants to *display* armed-ness derives from here, so the
   * composer and the send can no longer disagree.
   */
  const armed = $derived.by(() => {
    const selection = providersStore.active;
    if (!selection || !activeProvider) return null;
    return { selection, provider: activeProvider };
  });

  /**
   * What the picker chip shows. Built from {@link armed} — the ARMED pair —
   * and never from `modelGroups`.
   *
   * The old chip searched the fetched listings for the selected key, so a
   * provider whose `GET /models` failed (`items: []`) rendered the amber "No
   * model selected" placeholder while `canSend` stayed true and Send still went
   * to that provider. `unconfirmed` is the honest version of that state: the
   * selection is shown as armed, with a distinct warning that we could not
   * confirm the model against the endpoint.
   *
   * A provider with no group yet (the first listing round hasn't landed) is NOT
   * flagged — "we haven't asked yet" is not "we asked and it wasn't there", and
   * flashing a warning on every launch would train the user to ignore it.
   */
  const armedSelection = $derived.by((): ArmedSelection | null => {
    if (!armed) return null;
    const { selection, provider } = armed;
    const key = modelKey(selection.providerId, selection.model);
    const group = modelGroups.find((g) => g.id === provider.id);
    const listed = group?.items.some((m) => m.key === key) ?? true;
    return {
      key,
      model: selection.model,
      provider: provider.name,
      // `isPrivate` (the base URL), not `kind` (a user-typed label) — the same
      // predicate the backend stamps the turn's trust zone with.
      kind: provider.isPrivate ? "local" : "cloud",
      unconfirmed: listed
        ? null
        : (group?.notice ??
          "this endpoint's model list doesn't currently include it"),
    };
  });

  // The send color reflects the actual route commitment we can make before a
  // turn starts: explicit local/cloud models get their own colors, Auto stays
  // amber because the privacy filter decides — and "nothing selected" is its
  // OWN state, never folded into the amber filter state. An unarmed composer
  // is not a routing decision waiting to happen; it is a turn that cannot go
  // anywhere, and it must look different from one that can.
  type SendRoute = "local" | "public" | "filter" | "unset";
  const sendRoute = $derived.by((): SendRoute => {
    // Same `armed` the chip renders from. A failed model listing does not
    // disarm anything, and must not be able to make these two disagree.
    if (!armed) return "unset";
    if (binding === "auto") return "filter";
    // `isPrivate`, not `kind` — same reason as the picker groups above. A
    // Custom-kind provider pointed at a public API would otherwise turn the
    // Send button green and read "Send via local model".
    if (binding === "private" || armed.provider.isPrivate) return "local";
    return "public";
  });
  const canSend = $derived(sendRoute !== "unset");
  const SEND_ROUTE_LABEL: Record<SendRoute, string> = {
    local: "local model",
    public: "public model",
    filter: "privacy filter",
    unset: "no model selected",
  };
  const SEND_ROUTE_CLASS: Record<SendRoute, string> = {
    local: "bg-local text-white hover:brightness-110",
    public: "bg-cloud text-white hover:brightness-110",
    filter: "bg-warn text-[#231f16] hover:brightness-110",
    unset: "bg-surface-2 text-text-3 cursor-not-allowed",
  };
  const knotState = $derived.by((): KnotState => {
    if (sendRoute === "local") return "local";
    if (sendRoute === "public") return "cloud";
    if (sendRoute === "unset") return "idle";
    return "filter";
  });

  // A refusal or a lost selection, shown right under the composer. Cleared as
  // soon as the user makes a valid choice.
  let composerError = $state<string | null>(null);
  const composerNotice = $derived(composerError ?? providersStore.activeSelectionLost);

  // The backend does not expose full token accounting yet. The context panel
  // therefore reports only a clearly-labelled estimate for text already in the
  // visible conversation, and marks every unmetered source as such.
  const contextUsage = $derived.by(() => {
    const messages = $activeConversation?.messages ?? [];
    const estimate = (text: string) => Math.ceil(Array.from(text).length / 4);
    const input = messages
      .filter((message) => message.role === "user")
      .reduce((total, message) => total + estimate(message.content), 0);
    const output = messages
      .filter((message) => message.role === "assistant")
      .reduce((total, message) => total + estimate(message.content), 0);
    return { input, output, total: input + output };
  });
  // The frontend has no model-specific window metadata yet. Use the same
  // conservative 8k presentation baseline as the model-fit UI, and make the
  // number explicit in the detail popover rather than implying it is exact.
  const CONTEXT_METER_WINDOW = 8192;
  const CONTEXT_RING_RADIUS = 8.25;
  const CONTEXT_RING_CIRCUMFERENCE = 2 * Math.PI * CONTEXT_RING_RADIUS;
  const contextRing = $derived.by(() => {
    const filled = Math.min(contextUsage.total / CONTEXT_METER_WINDOW, 1);
    return { dashOffset: CONTEXT_RING_CIRCUMFERENCE * (1 - filled) };
  });

  type DictationAlternative = { transcript: string };
  type DictationResult = { isFinal: boolean; [index: number]: DictationAlternative };
  type DictationEvent = { resultIndex: number; results: ArrayLike<DictationResult> };
  type DictationRecognition = {
    continuous: boolean;
    interimResults: boolean;
    lang: string;
    onresult: ((event: DictationEvent) => void) | null;
    onerror: (() => void) | null;
    onend: (() => void) | null;
    start: () => void;
    stop: () => void;
  };
  type DictationConstructor = new () => DictationRecognition;
  let recognition: DictationRecognition | null = null;
  let voiceNoticeTimer: ReturnType<typeof setTimeout> | null = null;

  function showVoiceNotice(message: string) {
    voiceNotice = message;
    if (voiceNoticeTimer) clearTimeout(voiceNoticeTimer);
    voiceNoticeTimer = setTimeout(() => (voiceNotice = null), 3500);
  }

  function toggleVoiceInput() {
    composerPopover = null;
    permissionOpen = false;
    if (listening) {
      recognition?.stop();
      return;
    }
    const voiceWindow = window as typeof window & {
      SpeechRecognition?: DictationConstructor;
      webkitSpeechRecognition?: DictationConstructor;
    };
    const Recognition = voiceWindow.SpeechRecognition ?? voiceWindow.webkitSpeechRecognition;
    if (!Recognition) {
      showVoiceNotice("Voice dictation is not available in this preview.");
      return;
    }

    const nextRecognition = new Recognition();
    nextRecognition.continuous = false;
    nextRecognition.interimResults = false;
    nextRecognition.lang = navigator.language || "en-US";
    nextRecognition.onresult = (event) => {
      let transcript = "";
      for (let index = event.resultIndex; index < event.results.length; index += 1) {
        const result = event.results[index];
        if (result?.isFinal) transcript += result[0]?.transcript ?? "";
      }
      if (!transcript.trim()) return;
      draft = draft.trimEnd() ? `${draft.trimEnd()} ${transcript.trim()}` : transcript.trim();
      autoresize();
    };
    nextRecognition.onerror = () => showVoiceNotice("Voice dictation could not start.");
    nextRecognition.onend = () => (listening = false);
    recognition = nextRecognition;
    listening = true;
    try {
      nextRecognition.start();
    } catch {
      listening = false;
      showVoiceNotice("Voice dictation could not start.");
    }
  }

  // ── Routing: report a real message's trust zone.
  //
  // Every branch below reads a PERSISTED backend fact about that turn. Nothing
  // here consults the live provider registry, and nothing falls through to a
  // reassuring default.
  //
  // The bug this replaces did both: it looked the served provider up in
  // `providersStore` and returned `"local"` whenever it couldn't find one. So
  // a turn genuinely served by a public cloud endpoint rendered as a green
  // "Local" badge the moment that endpoint was deleted — the privacy signal
  // inverted. The trust zone of a past turn is a fact about the past, and it
  // now arrives from the backend stamped on the turn (`served_by.zone`).
  function messageRoute(m: Message): Route {
    // H-12: `gate_confirm` also means nothing left the machine (pending the
    // user's one-send confirmation), so it reads as "held" too.
    if (
      m.error_source === "gate" ||
      m.error_source === "gate_confirm" ||
      m.routing_decision === "block"
    )
      return "blocked";

    // The authoritative answer: the zone the backend stamped on this turn when
    // it ran, from the same `is_cloud` the privacy gate itself was given.
    const zone = m.served_by?.zone;
    if (zone === "cloud") return "cloud";
    if (zone === "local") return "local";

    // No stamped zone (a row older than the stamp). The only remaining
    // evidence is the persisted routing decision — also a backend fact written
    // with the row, not live state:
    //   route_local / tool_reroute_local — the gate rerouted the turn, and
    //     `enforce_local_routing` structurally proves the target was
    //     is_local() && is_private().
    //   redact_send — the safe remainder went to the cloud (sensitive spans
    //     stayed local); the event bar explains the redaction.
    if (
      m.routing_decision === "route_local" ||
      m.routing_decision === "tool_reroute_local"
    )
      return "local";
    if (m.routing_decision === "redact_send") return "cloud";

    // "allow" with no stamped zone tells us the gate permitted the turn, not
    // where it went. Say so.
    return "unknown";
  }

  /** The endpoint that actually served a turn: provider name + host.
   *  Falls back to the persisted provider id when the provider has since been
   *  removed — an id the user can still recognise beats inventing a name. */
  function servedByLabel(m: Message): string | null {
    const served = m.served_by;
    if (!served) return null;
    const host = served.base_url ? hostOf(served.base_url) : null;
    if (served.provider_name && host) return `${served.provider_name} (${host})`;
    return served.provider_name ?? served.provider_id;
  }

  function hostOf(baseUrl: string): string {
    try {
      return new URL(baseUrl).host;
    } catch {
      return baseUrl;
    }
  }

  // Spec: "the UI must show, per turn, which provider+endpoint actually served
  // it". The model string alone never says WHERE it ran — and on a reroute or
  // a redacted send the serving endpoint is a different one than the composer
  // shows, which is exactly the case worth surfacing.
  // An exhaustive map, not a chained ternary: the ternary this replaces had an
  // `else` arm, so adding a fourth Route silently labelled it "Held" — an
  // unknown route would have claimed the turn was blocked.
  const ROUTE_PREFIX: Record<Route, string> = {
    local: "Local",
    cloud: "Cloud",
    blocked: "Held",
    unknown: "Unknown route",
  };

  function routeLabel(m: Message): string {
    const parts = [ROUTE_PREFIX[messageRoute(m)]];
    const served = servedByLabel(m);
    if (served) parts.push(served);
    if (m.model) parts.push(m.model);
    return parts.join(" · ");
  }

  /** Full endpoint URL for the badge tooltip — the detail that doesn't fit in
   *  a calm chip but settles "where did this go?" outright. */
  function routeTitle(m: Message): string | undefined {
    if (messageRoute(m) === "unknown") {
      // Say what we don't know and why, rather than leaving a bare chip. This
      // is the honest end of the old silent "Local" default.
      return (
        "This turn was recorded before Lost Harness stamped the trust zone on " +
        "each turn, so whether it stayed on this machine can't be confirmed." +
        (m.served_by?.base_url ? ` Endpoint now: ${m.served_by.base_url}` : "")
      );
    }
    return m.served_by?.base_url ?? undefined;
  }

  // Only show a routing badge once a real decision exists — never on a
  // still-streaming turn, and never fabricated for a plain model/network
  // error (those aren't a privacy/routing outcome).
  function hasRoutingSignal(m: Message): boolean {
    return (
      !m.streaming &&
      (m.routing_decision != null ||
        m.error_source === "gate" ||
        m.error_source === "gate_confirm")
    );
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
    // Fail closed. The composer used to read the store and send whatever was
    // there, including `provider_id: ""` / `model: ""`. A turn goes to the
    // endpoint the user picked, or it does not go — it is never guessed.
    const selection = providersStore.active;
    if (!selection) {
      composerError = NO_ENDPOINT_SELECTED;
      return; // draft is deliberately kept — nothing was sent
    }
    composerError = null;
    isSending = true;
    draft = "";
    autoresize();
    try {
      await sendChatMessage(
        content,
        selection.providerId,
        selection.model,
        $activeConversation ? undefined : binding,
        mode,
      );
    } catch (err) {
      // sendMessage surfaces model/gate failures inline on the assistant row
      // and does not throw; reaching here means nothing was sent at all (the
      // store's own endpoint precondition, or a failed conversation create).
      // Give the user their text back rather than swallowing it.
      composerError = err instanceof Error ? err.message : String(err);
      draft = content;
      autoresize();
    } finally {
      isSending = false;
      textareaEl?.focus();
    }
  }

  // H-12: the user's answer to a `"gate_confirm"` hold — a `Public`-bound
  // message that hit the un-tunable structured-secret floor. Records a
  // single-use, expiring authorisation for THIS EXACT text and re-sends it.
  // Deliberately re-sends `held_content` rather than the composer draft: the
  // grant is fingerprinted over the held text, so anything else would (and
  // should) re-prompt.
  let confirming = $state(false);
  async function confirmAndResend(heldContent: string) {
    if (confirming || isSending) return;
    // Same fail-closed rule as handleSend. A confirmed send is still a send,
    // and the endpoint it goes to is exactly what the user is confirming.
    const selection = providersStore.active;
    if (!selection) {
      composerError = NO_ENDPOINT_SELECTED;
      return;
    }
    composerError = null;
    confirming = true;
    try {
      await confirmPublicSend(heldContent);
      await sendChatMessage(
        heldContent,
        selection.providerId,
        selection.model,
        $activeConversation ? undefined : binding,
        mode,
      );
    } catch (err) {
      composerError = err instanceof Error ? err.message : String(err);
    } finally {
      confirming = false;
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
  <Sidebar activeConv={$activeConversation?.name ?? "New chat"} engineState={knotState} />

  <main class="flex min-h-0 min-w-0 flex-col">
    <div
      class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]"
    >
      <div class="min-w-0 truncate text-[13.5px] font-semibold">
        {$activeConversation?.name ?? "New chat"}
      </div>

      <button
        type="button"
        onclick={() => void cycleBinding()}
        title={BINDING_DESC[binding]}
        aria-label="Conversation binding — click to switch"
        disabled={bindingSaving}
        class="inline-flex h-7 cursor-pointer items-center gap-[7px] rounded-[14px] border border-border-strong bg-surface px-3 text-[12px] font-semibold tracking-[0.02em] text-text disabled:cursor-wait disabled:opacity-60"
      >
        {@render dot(binding === "public" ? "bg-cloud" : "bg-local")}
        {BINDING_LABEL[binding]}
      </button>
      {#if bindingError}
        <span class="max-w-56 truncate text-[11px] text-blocked" title={bindingError}>{bindingError}</span>
      {/if}

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
            {:else if m.routing_decision === "redact_send"}
              <PrivacyEventBar kind="kept" title="Sent the safe parts only">
                Sensitive details were blacked out and kept on this Mac; only the
                redacted remainder went to the cloud, and the reply was restored
                locally. Open “Why” to see exactly what was held back.
              </PrivacyEventBar>
            {:else if m.error_source === "gate_confirm"}
              <!-- H-12: an ACTIONABLE hold. This chat is set to Public, so the
                   user already opted into cloud — but the un-tunable floor found
                   a secret / identifier, and a deliberate one-time confirmation
                   is required. The link authorises exactly ONE send of this text
                   and expires; it never becomes a standing allow. -->
              {#snippet confirmLinks()}
                <button
                  type="button"
                  class="underline underline-offset-2 disabled:opacity-50"
                  disabled={confirming || isSending}
                  onclick={() => confirmAndResend(m.held_content ?? "")}
                >
                  {confirming ? "Sending…" : "Send it this once"}
                </button>
                <button
                  type="button"
                  class="underline underline-offset-2"
                  onclick={toggleWhy}
                >
                  What tripped it
                </button>
              {/snippet}
              <PrivacyEventBar
                kind="stop"
                title="Held — this looks like a secret"
                links={m.held_content ? confirmLinks : undefined}
              >
                {m.error ??
                  "The privacy gate wants your confirmation before this leaves the machine."}
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
                title={routeTitle(m)}
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

    {#if memoryNote}
      <div class="flex-shrink-0 px-5">
        <div
          class="mx-auto flex max-w-[700px] items-center gap-1.5 text-[11.5px] text-text-3"
        >
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
            <path d="M12 8v4l3 3M12 3a9 9 0 100 18 9 9 0 000-18z" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
          {memoryNote}
        </div>
      </div>
    {/if}

    <div class="flex-shrink-0 px-5 pb-4 pt-2">
      {#if composerNotice}
        <!-- A refused send, or an endpoint that went away under the user.
             Never silent: the alternative to saying this out loud is a
             message going somewhere they didn't choose. -->
        <div class="mx-auto mb-1.5 max-w-[700px]">
          <p
            role="status"
            class="flex items-start gap-1.5 rounded-[var(--r)] bg-warn-soft px-2.5 py-1.5 text-[11.5px] leading-[1.35] text-warn"
          >
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true" class="mt-[1px] shrink-0">
              <path d="M12 8v5M12 16.5v.1" stroke-linecap="round" />
              <circle cx="12" cy="12" r="9" />
            </svg>
            {composerNotice}
          </p>
        </div>
      {/if}
      <div
        class="mx-auto flex max-w-[700px] flex-col rounded-[24px] border border-border-strong bg-surface px-[15px] py-1.5 shadow-[var(--shadow)] transition-colors duration-100 focus-within:border-[color-mix(in_srgb,var(--accent)_55%,var(--border-strong))]"
      >
        <textarea
          bind:this={textareaEl}
          bind:value={draft}
          rows="1"
          placeholder="Message Lost Harness…"
          onkeydown={handleKeydown}
          oninput={autoresize}
          class="min-h-[25px] max-h-[150px] w-full min-w-0 flex-1 resize-none border-0 bg-transparent px-[2px] py-0 text-[15px] leading-[1.55] text-text outline-none placeholder:text-text-3"
        ></textarea>

        <div class="mt-1 flex items-center justify-between gap-3">
          <div class="flex items-center gap-1">
            <div bind:this={attachmentEl} class="relative flex items-center">
            <button
              type="button"
              aria-label="Attachments"
              aria-expanded={composerPopover === "attachments"}
              title="Attachments"
              onclick={() => {
                composerPopover = composerPopover === "attachments" ? null : "attachments";
                permissionOpen = false;
              }}
              class="grid h-[36px] w-[36px] place-items-center rounded-full border border-transparent text-text-2 transition-[background-color,color] hover:bg-surface-hover hover:text-text focus-visible:border-accent focus-visible:outline-none"
            >
              <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
                <path d="M12 5v14M5 12h14" stroke-linecap="round" />
              </svg>
            </button>
            {#if composerPopover === "attachments"}
              <div class="absolute bottom-[calc(100%+10px)] left-0 z-40 w-[240px] rounded-[var(--r-lg)] border border-border-strong bg-surface px-3 py-2.5 shadow-[var(--shadow-pop)]">
                <div class="text-[12px] font-semibold text-text">Attachments</div>
                <p class="mt-1 text-[11px] leading-[1.4] text-text-3">
                  File and photo attachment sending has not been connected to this build yet.
                </p>
              </div>
            {/if}
          </div>

            <div bind:this={permissionEl} class="relative flex items-center">
              <button
                type="button"
                onclick={() => {
                  permissionOpen = !permissionOpen;
                  composerPopover = null;
                }}
                title={`Permissions: ${MODE_LABEL[mode]}`}
                aria-label="Permission controls"
                aria-expanded={permissionOpen}
                class="relative grid h-[36px] w-[36px] cursor-pointer place-items-center rounded-full transition-[background-color,color] {mode === 'plan'
                  ? 'bg-warn-soft text-warn'
                  : mode === 'accept_edits'
                    ? 'bg-local-soft text-local'
                    : 'bg-transparent text-text-3 hover:bg-surface-hover hover:text-text'}"
              >
                <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" aria-hidden="true">
                  <path d="M12 3 19 6v5c0 4.8-2.9 8.2-7 10-4.1-1.8-7-5.2-7-10V6l7-3Z" />
                  {#if mode === "plan"}
                    <path d="M9 12h6" stroke-linecap="round" />
                  {:else if mode === "accept_edits"}
                    <path d="m9 12 2 2 4-4" stroke-linecap="round" stroke-linejoin="round" />
                  {:else}
                    <path d="M12 8.5v3.2M12 15.3v.1" stroke-linecap="round" />
                  {/if}
                </svg>
              </button>
              {#if permissionOpen}
                <div class="absolute bottom-[calc(100%+10px)] left-0 z-50 w-[248px] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface p-1.5 shadow-[var(--shadow-pop)]">
                  <div class="px-2.5 pb-1.5 pt-1 text-[10px] font-semibold uppercase tracking-[0.07em] text-text-3">Permissions</div>
                  {#each MODE_OPTIONS as option (option.id)}
                    <button
                      type="button"
                      aria-pressed={mode === option.id}
                      onclick={() => selectMode(option.id)}
                      class="flex w-full items-start gap-2.5 rounded-[var(--r)] px-2.5 py-2 text-left transition-[0.1s] {mode === option.id
                        ? 'bg-surface-2 text-text'
                        : 'text-text-2 hover:bg-surface-hover hover:text-text'}"
                    >
                      <span class="mt-[5px] h-1.5 w-1.5 shrink-0 rounded-full {option.id === 'plan' ? 'bg-warn' : option.id === 'accept_edits' ? 'bg-local' : 'bg-text-3'}"></span>
                      <span>
                        <span class="block text-[12px] font-semibold">{option.label}</span>
                        <span class="mt-0.5 block text-[10.5px] leading-[1.35] text-text-3">{option.description}</span>
                      </span>
                    </button>
                  {/each}
                </div>
              {/if}
            </div>
          </div>

          <div class="flex items-center gap-1.5">
            <div bind:this={contextEl} class="relative flex items-center">
              <button
                type="button"
                aria-label="Conversation context usage"
                aria-expanded={composerPopover === "context"}
                title="Conversation context"
                onclick={() => {
                  composerPopover = composerPopover === "context" ? null : "context";
                  permissionOpen = false;
                }}
                class="grid h-[36px] w-[36px] place-items-center rounded-full border border-transparent text-text-3 transition-[background-color,color] hover:bg-surface-hover hover:text-text-2 focus-visible:border-accent focus-visible:outline-none"
              >
                <svg width="21" height="21" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                  <circle cx="12" cy="12" r={CONTEXT_RING_RADIUS} stroke="currentColor" stroke-width="2" opacity=".25" />
                  <circle
                    cx="12"
                    cy="12"
                    r={CONTEXT_RING_RADIUS}
                    stroke="var(--accent)"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-dasharray={CONTEXT_RING_CIRCUMFERENCE}
                    stroke-dashoffset={contextRing.dashOffset}
                    transform="rotate(-90 12 12)"
                    class="[transition:stroke-dashoffset_300ms_ease]"
                  />
                </svg>
              </button>
              {#if composerPopover === "context"}
                <div class="absolute bottom-[calc(100%+10px)] right-0 z-40 w-[310px] rounded-[var(--r-lg)] border border-border-strong bg-surface p-3 shadow-[var(--shadow-pop)]">
                  <div class="flex items-baseline justify-between gap-3">
                    <span class="text-[12px] font-semibold text-text">Conversation context</span>
                    <span class="whitespace-nowrap text-[10px] text-text-3">~{contextUsage.total} / 8k text tokens</span>
                  </div>
                  <p class="mt-1 text-[10.5px] leading-[1.35] text-text-3">
                    Text is estimated from the visible conversation. The rest is shown only when it is metered.
                  </p>
                  <div class="mt-3 grid grid-cols-[1fr_auto] gap-x-3 gap-y-1.5 text-[11px]">
                    <span class="text-text-2">System prompt</span><span class="text-text-3">Not metered</span>
                    <span class="text-text-2">Tools</span><span class="text-text-3">Not metered</span>
                    <span class="text-text-2">Skills</span><span class="text-text-3">Not metered</span>
                    <span class="text-text-2">MCP</span><span class="text-text-3">Not metered</span>
                    <span class="text-text-2">Files</span><span class="text-text-3">Not metered</span>
                    <span class="text-text-2">Photos</span><span class="text-text-3">Not metered</span>
                    <span class="text-text-2">Input</span><span class="text-text">~{contextUsage.input}</span>
                    <span class="text-text-2">Output</span><span class="text-text">~{contextUsage.output}</span>
                  </div>
                </div>
              {/if}
            </div>

            <ModelPicker
              groups={modelGroups}
              selection={armedSelection}
              onchange={handleModelChange}
              onopen={() => {
                // OPENING only, and NO network. This used to fire on open AND
                // close and kick off a cache-bypassing `GET {base_url}/models`
                // — bearer key attached — to every configured provider,
                // including cloud ones, on every picker click. Listing is now
                // either cached, driven by the provider list changing, or
                // explicitly requested via the Refresh button below.
                composerPopover = null;
                permissionOpen = false;
              }}
              refreshing={modelsRefreshing}
              onrefresh={() => void refreshModelGroups()}
            />

            <div class="relative flex items-center">
              <button
                type="button"
                aria-label={listening ? "Stop voice input" : "Start voice input"}
                aria-pressed={listening}
                title={listening ? "Listening… click to stop" : "Voice input"}
                onclick={toggleVoiceInput}
                class="grid h-[36px] w-[36px] place-items-center rounded-full border border-transparent transition-[background-color,color] focus-visible:border-accent focus-visible:outline-none {listening
                  ? 'bg-accent-soft text-accent'
                  : 'text-text-3 hover:bg-surface-hover hover:text-text-2'}"
              >
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
                  <rect x="9" y="3" width="6" height="11" rx="3" />
                  <path d="M6.5 11.5a5.5 5.5 0 0 0 11 0M12 17v4M8.5 21h7" stroke-linecap="round" />
                </svg>
              </button>
              {#if voiceNotice}
                <div class="absolute bottom-[calc(100%+10px)] right-0 z-40 w-[230px] rounded-[var(--r-lg)] border border-border-strong bg-surface px-3 py-2 text-[11px] text-text-2 shadow-[var(--shadow-pop)]">
                  {voiceNotice}
                </div>
              {/if}
            </div>

            {#if $streamingMessage && $streamingMessage.conversationId === $activeConversationId}
              <!-- C7 cooperative cancel: while THIS conversation streams, the
                   send slot becomes a Stop button (backend persists the partial
                   with aborted:true and ends the stream). -->
              <button
                type="button"
                aria-label="Stop generating"
                onclick={() => void cancelActiveStream()}
                class="grid h-[36px] w-[36px] place-items-center rounded-full bg-surface-2 text-text transition-[transform,filter] hover:scale-[1.03] hover:bg-surface-hover"
              >
                <svg width="15" height="15" viewBox="0 0 24 24" fill="currentColor">
                  <rect x="6" y="6" width="12" height="12" rx="1.5" />
                </svg>
              </button>
            {:else}
              <button
                type="button"
                disabled={!canSend}
                aria-label={canSend
                  ? `Send via ${SEND_ROUTE_LABEL[sendRoute]}`
                  : "Can't send — pick a model first"}
                title={canSend
                  ? `Send via ${SEND_ROUTE_LABEL[sendRoute]}`
                  : "Pick a model in the picker before sending"}
                onclick={handleSend}
                class="grid h-[36px] w-[36px] place-items-center rounded-full transition-[transform,filter] enabled:hover:scale-[1.03] {SEND_ROUTE_CLASS[sendRoute]}"
              >
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                  <path d="M12 19V5M5 12l7-7 7 7" stroke-linecap="round" stroke-linejoin="round" />
                </svg>
              </button>
            {/if}
          </div>
        </div>
      </div>
    </div>

    <AppStatusBar {binding} />
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
              This profile's workspace
            </div>
            {#if wsError}
              <div class="px-0.5 py-1 text-[12px] text-red-400">{wsError}</div>
            {:else if wsLoading}
              <p class="px-0.5 py-1 text-[12px] text-text-3">Loading…</p>
            {:else if wsEntries.length === 0}
              <p class="px-0.5 py-1 text-[12px] text-text-3">
                Nothing here yet — files the assistant reads or writes for this
                profile land in its workspace.
              </p>
            {:else}
              <div class="flex flex-col gap-1">
                {#each wsEntries.slice(0, 8) as f (f.name)}
                  <div class="flex items-center gap-2.5 rounded-[var(--r)] px-2.5 py-[7px]">
                    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--text-3)" stroke-width="1.7" class="shrink-0">
                      {#if f.is_dir}
                        <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
                      {:else}
                        <path d="M6 2h9l5 5v15H6z" />
                        <path d="M15 2v5h5" />
                      {/if}
                    </svg>
                    <span class="min-w-0 flex-1 truncate text-[12.5px] text-text">{f.name}</span>
                  </div>
                {/each}
              </div>
            {/if}
            <button
              type="button"
              onclick={() => nav.go("files")}
              class="mt-2 border-0 bg-transparent p-0 px-0.5 text-[12px] font-semibold text-text underline"
            >
              Open the Files browser
            </button>
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
