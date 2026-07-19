<script lang="ts">
  // Settings — a 2/3 modal over a blurred app: submenu nav (Routing, Privacy
  // guard, Models, Memory, Appearance), a per-profile editable memory viewer, and
  // live accent/theme/appearance controls. Closing returns to the main screen.
  // Ported from the React Settings screen (templates/settings/Settings.dc.html).
  import { nav } from "../nav.svelte";
  import Button from "../components/Button.svelte";
  import ChatMessage from "../components/ChatMessage.svelte";
  import IconButton from "../components/IconButton.svelte";
  import SegmentedControl from "../components/SegmentedControl.svelte";
  import Select from "../components/Select.svelte";
  import SettingRow from "../components/SettingRow.svelte";
  import Toggle from "../components/Toggle.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import {
    providersStore,
    addProvider,
    removeProvider,
    setActiveModel,
    fetchModels,
    type Provider,
    type ProviderKind,
  } from "$lib/stores/providers.svelte";
  import { modelsForProvider } from "$lib/stores/provider-catalog";
  import { theme, applyTheme, type Theme } from "$lib/stores/settings";
  import { activeProfileId } from "$lib/stores/profiles";
  import {
    listToolRules,
    deleteToolRule,
    type ToolRule,
    listMemory,
    saveMemory,
    deleteMemory,
    setMemoryPinned,
    type MemoryInfo,
    getMemorySettings,
    setMemorySettings,
    getClassifierSettings,
    setClassifierSettings,
    setRedactionEnabled,
    resetClassifierSettings,
    getUsageSummary,
    type UsageSummary,
    listSkills,
    setSkillApproval,
    deleteSkill,
    getSkillReflectEnabled,
    setSkillReflectEnabled,
    type SkillInfo,
    listSeatBindings,
    setSeatBinding,
    deleteSeatBinding,
    type SeatBinding,
    listAgentTypes,
    setAgentTypeApproval,
    deleteAgentType,
    type AgentType,
    installPack,
    listModelCatalog,
    downloadModel,
    probeHardware,
    type CatalogModel,
    type HardwareProfile,
    listLocalModels,
    removeLocalModel,
    type LocalModel,
  } from "$lib/api/tauri";

  type Section = "routing" | "privacy" | "permissions" | "models" | "memory" | "skills" | "agents" | "usage" | "appearance";
  const SECTIONS: [Section, string][] = [
    ["routing", "Routing"],
    ["privacy", "Privacy guard"],
    ["permissions", "Permissions"],
    ["models", "Models"],
    ["memory", "Memory"],
    ["skills", "Skills"],
    ["agents", "Agent types"],
    ["usage", "Usage"],
    ["appearance", "Appearance"],
  ];


  const ACCENTS = ["#5f74e0", "#3fa87d", "#4a97cf", "#c49a55", "#b06fc2", "#d0685f", "#5fb8b0", "#8a8a93"];

  // models — quick-add presets mirror the old (already-wired) ProviderSettings
  // component's PROVIDER_CATALOG entries.
  interface ProviderFormState {
    id: string | null;
    name: string;
    baseUrl: string;
    apiKey: string;
    kind: ProviderKind;
    /** Q1: whether this endpoint supports OpenAI-style native structured tool calls. */
    supportsNativeTools: boolean;
  }
  const EMPTY_PROVIDER_FORM: ProviderFormState = {
    id: null,
    name: "",
    baseUrl: "",
    apiKey: "",
    kind: "cloud",
    supportsNativeTools: false,
  };
  const QUICK_PROVIDER_PRESETS: Array<{
    id: string;
    name: string;
    baseUrl: string;
    kind: ProviderKind;
  }> = [
    { id: "openai", name: "OpenAI", baseUrl: "https://api.openai.com/v1", kind: "cloud" },
    { id: "anthropic", name: "Anthropic", baseUrl: "https://api.anthropic.com/v1", kind: "cloud" },
    { id: "openrouter", name: "OpenRouter", baseUrl: "https://openrouter.ai/api/v1", kind: "cloud" },
    { id: "lmstudio", name: "LM Studio", baseUrl: "http://localhost:1234/v1", kind: "local" },
    { id: "ollama", name: "Ollama", baseUrl: "http://127.0.0.1:11434/v1", kind: "local" },
  ];
  const PROVIDER_KIND_OPTIONS = [
    { value: "cloud", label: "Cloud (public endpoint)" },
    { value: "local", label: "Local (loopback / LAN / tailnet)" },
    { value: "custom", label: "Custom (other)" },
  ];

  const goMain = () => nav.go("main");
  let section = $state<Section>("routing");

  // routing
  let defaultBinding = $state("auto");
  let uncertainty = $state(true);
  // privacy — real per-profile classifier thresholds (PLAN §11), loaded live
  let guard = $state(true);
  let classifierStrictness = $state(50);
  let classifierBand = $state<"narrow" | "medium" | "wide">("medium");
  let classifierRedaction = $state(true);
  let classifierLoading = $state(false);
  let classifierError = $state<string | null>(null);
  let classifierSaving = $state(false);
  // models
  let providerForm = $state<ProviderFormState | null>(null);
  let showProviderKey = $state(false);
  let confirmRemoveProviderId = $state<string | null>(null);
  let savingProvider = $state(false);
  // Fetched model lists per provider id (populated lazily below).
  let modelsByProvider = $state<Record<string, string[]>>({});
  // memory — real facts for the active profile (PLAN §9)
  let memoryMode = $state("walled");
  let semanticSearchEnabled = $state(true);
  let memSettingsSaving = $state(false);
  let memSettingsError = $state<string | null>(null);
  let memoryItems = $state<MemoryInfo[]>([]);
  let memoryLoading = $state(false);
  let memDraft = $state("");
  let memSaving = $state(false);
  let memNote = $state<string | null>(null);
  let confirmForgetId = $state<string | null>(null);
  // appearance
  let accent = $state("#5f74e0");
  let tone = $state("neutral");
  let density = $state("cozy");
  let fontSize = $state(13.5);
  let motion = $state(true);
  // permissions — persisted "Always allow" tool rules (Q8), read live from the
  // active profile's SQLite `tool_rules`. Revoking re-prompts on the next call.
  let toolRules = $state<ToolRule[]>([]);
  let rulesLoading = $state(false);
  let rulesError = $state<string | null>(null);
  // usage — the profile's model-call cost ledger roll-up (Wave 3.2)
  let usage = $state<UsageSummary | null>(null);
  let usageLoading = $state(false);
  let usageError = $state<string | null>(null);
  let confirmRevokeId = $state<string | null>(null);
  // skills — the global saved-skills store + its review gate (Wave 4.1)
  let skillItems = $state<SkillInfo[]>([]);
  let skillsLoading = $state(false);
  let skillsError = $state<string | null>(null);
  let confirmDeleteSkillId = $state<string | null>(null);
  let expandedSkillId = $state<string | null>(null);
  // agent types — declarative personas delegate can dispatch to (Wave 4.3)
  let agentTypes = $state<AgentType[]>([]);
  let agentsLoading = $state(false);
  let agentsError = $state<string | null>(null);
  let confirmDeleteAgentId = $state<string | null>(null);
  // capability packs — install a bundle of skills+agents+cron (Wave 4.5)
  let packJson = $state("");
  let packInstalling = $state(false);
  let packNote = $state<string | null>(null);
  let packOpen = $state(false);
  let skillReflectEnabled = $state(false);
  let skillReflectSaving = $state(false);
  // model catalog — downloadable local models sized to this machine (Wave 5.3 / M8)
  let catalogModels = $state<CatalogModel[]>([]);
  let hardware = $state<HardwareProfile | null>(null);
  let modelDownloadStatus = $state<Record<string, string>>({});
  let localModels = $state<LocalModel[]>([]);
  let confirmRemoveModelId = $state<string | null>(null);
  // seats — per-profile model-seat bindings (Wave 3.1)
  let seatBindings = $state<SeatBinding[]>([]);
  let seatName = $state("");
  let seatProviderId = $state("");
  let seatModel = $state("");
  let seatError = $state<string | null>(null);
  let seatSaving = $state(false);
  let confirmUnbindSeat = $state<string | null>(null);

  let activeLabel = $derived(SECTIONS.find(([id]) => id === section)![1]);

  // Name + URL required for save to be enabled (mirrors ProviderSettings.svelte).
  const canSaveProvider = $derived.by(() => {
    if (!providerForm) return false;
    const name = providerForm.name.trim();
    const url = providerForm.baseUrl.trim();
    if (!name || !url) return false;
    if (!/^https?:\/\//.test(url)) return false;
    return true;
  });

  // Lazily fetch each provider's model list once. Falls back to the baked-in
  // catalog (provider-catalog.ts) if the IPC call comes back empty — e.g. no
  // Tauri shell in browser dev mode, or a provider the catalog doesn't know
  // about yet whose /models call hasn't resolved.
  $effect(() => {
    for (const p of providersStore.providers) {
      if (p.id in modelsByProvider) continue;
      fetchModels(p.id).then((models) => {
        modelsByProvider = { ...modelsByProvider, [p.id]: models };
      });
    }
  });

  // Load the active profile's persisted rules whenever the Permissions pane is
  // open (and re-load if the profile changes under it). SqlitePolicySource reads
  // live on the backend, so a revoke here re-prompts on the tool's next call.
  $effect(() => {
    if (section !== "permissions") return;
    const profile = $activeProfileId;
    rulesLoading = true;
    rulesError = null;
    listToolRules(profile)
      .then((rules) => {
        toolRules = rules;
      })
      .catch((err) => {
        rulesError = String(err);
      })
      .finally(() => {
        rulesLoading = false;
      });
  });

  // Load the active profile's usage ledger roll-up when the Usage pane opens.
  $effect(() => {
    if (section !== "usage") return;
    const profile = $activeProfileId;
    usageLoading = true;
    usageError = null;
    getUsageSummary(profile)
      .then((s) => {
        usage = s;
      })
      .catch((err) => {
        usageError = String(err);
      })
      .finally(() => {
        usageLoading = false;
      });
  });

  // Load every saved skill when the Skills pane opens. Skills are global (not
  // profile-scoped), but re-run on profile change so the pane refreshes on nav.
  $effect(() => {
    if (section !== "skills") return;
    void $activeProfileId;
    skillsLoading = true;
    skillsError = null;
    listSkills()
      .then((items) => {
        skillItems = items;
      })
      .catch((err) => {
        skillsError = String(err);
      })
      .finally(() => {
        skillsLoading = false;
      });
    getSkillReflectEnabled()
      .then((v) => {
        skillReflectEnabled = v;
      })
      .catch(() => {});
  });

  // Load every agent-type persona when the Agent types pane opens.
  $effect(() => {
    if (section !== "agents") return;
    void $activeProfileId;
    agentsLoading = true;
    agentsError = null;
    listAgentTypes()
      .then((items) => {
        agentTypes = items;
      })
      .catch((err) => {
        agentsError = String(err);
      })
      .finally(() => {
        agentsLoading = false;
      });
  });

  // Load this profile's seat bindings + the model catalog when the Models pane opens.
  $effect(() => {
    if (section !== "models") return;
    const profile = $activeProfileId;
    listSeatBindings(profile)
      .then((rows) => {
        seatBindings = rows;
      })
      .catch((err) => {
        seatError = String(err);
      });
    probeHardware().then((h) => (hardware = h)).catch(() => {});
    listModelCatalog().then((m) => (catalogModels = m)).catch(() => {});
    listLocalModels().then((m) => (localModels = m)).catch(() => {});
  });

  async function startModelDownload(m: CatalogModel) {
    modelDownloadStatus = { ...modelDownloadStatus, [m.id]: "Downloading…" };
    try {
      await downloadModel(m.id);
      modelDownloadStatus = { ...modelDownloadStatus, [m.id]: "Downloaded ✓" };
      localModels = await listLocalModels();
    } catch (err) {
      modelDownloadStatus = { ...modelDownloadStatus, [m.id]: `Failed: ${String(err)}` };
    }
  }

  async function removeModel(id: string) {
    if (confirmRemoveModelId !== id) {
      confirmRemoveModelId = id;
      setTimeout(() => {
        if (confirmRemoveModelId === id) confirmRemoveModelId = null;
      }, 3000);
      return;
    }
    confirmRemoveModelId = null;
    await removeLocalModel(id);
    localModels = localModels.filter((m) => m.id !== id);
  }

  function fmtGB(bytes: number): string {
    return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} GB`;
  }

  // Load the active profile's classifier thresholds when the Privacy guard
  // pane opens (and re-load on profile change). The backend reads live per
  // send, so a change here takes effect on the next message.
  $effect(() => {
    if (section !== "privacy") return;
    const profile = $activeProfileId;
    classifierLoading = true;
    classifierError = null;
    getClassifierSettings(profile)
      .then((s) => {
        classifierStrictness = s.strictness;
        classifierBand = s.uncertainty_band;
        classifierRedaction = s.redaction_enabled;
      })
      .catch((err) => {
        classifierError = String(err);
      })
      .finally(() => {
        classifierLoading = false;
      });
  });

  async function toggleRedaction(enabled: boolean) {
    classifierRedaction = enabled;
    classifierSaving = true;
    classifierError = null;
    try {
      const s = await setRedactionEnabled($activeProfileId, enabled);
      classifierRedaction = s.redaction_enabled;
    } catch (err) {
      classifierError = String(err);
    } finally {
      classifierSaving = false;
    }
  }

  async function saveClassifierSettings() {
    classifierSaving = true;
    classifierError = null;
    try {
      const s = await setClassifierSettings(
        $activeProfileId,
        classifierStrictness,
        classifierBand,
      );
      // Reflect any server-side normalization back into the controls.
      classifierStrictness = s.strictness;
      classifierBand = s.uncertainty_band;
    } catch (err) {
      classifierError = String(err);
    } finally {
      classifierSaving = false;
    }
  }

  async function resetClassifier() {
    classifierSaving = true;
    classifierError = null;
    try {
      const s = await resetClassifierSettings($activeProfileId);
      classifierStrictness = s.strictness;
      classifierBand = s.uncertainty_band;
      classifierRedaction = s.redaction_enabled;
    } catch (err) {
      classifierError = String(err);
    } finally {
      classifierSaving = false;
    }
  }

  async function revokeRule(id: string) {
    // Two-click confirm, mirroring the provider-remove pattern above.
    if (confirmRevokeId !== id) {
      confirmRevokeId = id;
      setTimeout(() => {
        if (confirmRevokeId === id) confirmRevokeId = null;
      }, 3000);
      return;
    }
    confirmRevokeId = null;
    try {
      await deleteToolRule($activeProfileId, id);
      toolRules = toolRules.filter((r) => r.id !== id);
    } catch (err) {
      rulesError = String(err);
    }
  }

  function formatRuleDate(epochSeconds: number): string {
    return new Date(epochSeconds * 1000).toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
    });
  }

  function modelsFor(p: Provider): string[] {
    const fetched = modelsByProvider[p.id];
    if (fetched && fetched.length > 0) return fetched;
    return modelsForProvider(p).map((m) => m.name);
  }

  function providerDotColor(kind: ProviderKind): string {
    if (kind === "local") return "var(--local)";
    if (kind === "cloud") return "var(--cloud)";
    return "var(--text-3)";
  }

  function startAddProvider() {
    providerForm = { ...EMPTY_PROVIDER_FORM };
    showProviderKey = false;
  }
  function startAddProviderFromPreset(preset: (typeof QUICK_PROVIDER_PRESETS)[number]) {
    providerForm = {
      id: null,
      name: preset.name,
      baseUrl: preset.baseUrl,
      apiKey: "",
      kind: preset.kind,
      supportsNativeTools: false,
    };
    showProviderKey = false;
  }
  function startEditProvider(p: Provider) {
    if (providerForm?.id === p.id) {
      providerForm = null;
      return;
    }
    providerForm = {
      id: p.id,
      name: p.name,
      baseUrl: p.baseUrl,
      apiKey: "",
      kind: p.kind,
      supportsNativeTools: p.supportsNativeTools,
    };
    showProviderKey = false;
  }
  function cancelProviderForm() {
    providerForm = null;
  }
  async function saveProvider() {
    if (!providerForm || !canSaveProvider || savingProvider) return;
    savingProvider = true;
    try {
      await addProvider({
        id: providerForm.id ?? undefined,
        name: providerForm.name.trim(),
        baseUrl: providerForm.baseUrl.trim(),
        apiKey: providerForm.apiKey,
        kind: providerForm.kind,
        supportsNativeTools: providerForm.supportsNativeTools,
      });
      providerForm = null;
    } catch (err) {
      console.error("save provider failed", err);
    } finally {
      savingProvider = false;
    }
  }
  async function handleRemoveProvider(id: string) {
    // Two-click confirm: first click arms it, second click within 3s fires.
    if (confirmRemoveProviderId !== id) {
      confirmRemoveProviderId = id;
      setTimeout(() => {
        if (confirmRemoveProviderId === id) confirmRemoveProviderId = null;
      }, 3000);
      return;
    }
    confirmRemoveProviderId = null;
    await removeProvider(id);
  }

  function setTheme(v: string) {
    const t = v as Theme;
    theme.set(t);
    applyTheme(t);
  }

  // Load the active profile's real memory facts when the Memory pane is open
  // (and re-load if the profile changes under it).
  $effect(() => {
    if (section !== "memory") return;
    const profile = $activeProfileId;
    memoryLoading = true;
    listMemory(profile)
      .then((items) => {
        memoryItems = items;
      })
      .catch(() => {
        memoryItems = [];
      })
      .finally(() => {
        memoryLoading = false;
      });
  });

  // Load the active profile's memory settings (walled/shared + semantic
  // search) alongside the facts above, and re-load on profile change.
  $effect(() => {
    if (section !== "memory") return;
    const profile = $activeProfileId;
    memSettingsError = null;
    getMemorySettings(profile)
      .then((s) => {
        memoryMode = s.walled ? "walled" : "shared";
        semanticSearchEnabled = s.semantic_search_enabled;
      })
      .catch((err) => {
        memSettingsError = String(err);
      });
  });

  function reloadMemory() {
    listMemory($activeProfileId)
      .then((items) => (memoryItems = items))
      .catch(() => {});
  }

  async function setMemoryModeAndSave(mode: string) {
    memoryMode = mode;
    memSettingsSaving = true;
    memSettingsError = null;
    try {
      const s = await setMemorySettings($activeProfileId, semanticSearchEnabled, mode === "walled");
      memoryMode = s.walled ? "walled" : "shared";
      semanticSearchEnabled = s.semantic_search_enabled;
    } catch (err) {
      memSettingsError = String(err);
    } finally {
      memSettingsSaving = false;
    }
  }

  async function toggleSemanticSearch(enabled: boolean) {
    semanticSearchEnabled = enabled;
    memSettingsSaving = true;
    memSettingsError = null;
    try {
      const s = await setMemorySettings($activeProfileId, enabled, memoryMode === "walled");
      memoryMode = s.walled ? "walled" : "shared";
      semanticSearchEnabled = s.semantic_search_enabled;
    } catch (err) {
      memSettingsError = String(err);
    } finally {
      memSettingsSaving = false;
    }
  }

  async function addMemoryFact() {
    const content = memDraft.trim();
    if (!content || memSaving) return;
    memSaving = true;
    memNote = null;
    try {
      const res = await saveMemory($activeProfileId, content);
      if (res.sensitivity === "never_persist") {
        memNote =
          "That looked like a secret (e.g. a credential), so it was not saved anywhere — even locally.";
      } else if (res.sensitivity === "private_local") {
        memNote = "Saved to this device only — a cloud model will never see it.";
      } else {
        memNote = "Saved.";
      }
      memDraft = "";
      reloadMemory();
    } finally {
      memSaving = false;
    }
  }

  async function forgetMemory(id: string) {
    // Two-click confirm, mirroring the provider-remove pattern.
    if (confirmForgetId !== id) {
      confirmForgetId = id;
      setTimeout(() => {
        if (confirmForgetId === id) confirmForgetId = null;
      }, 3000);
      return;
    }
    confirmForgetId = null;
    await deleteMemory($activeProfileId, id);
    memoryItems = memoryItems.filter((m) => m.id !== id);
  }

  async function toggleMemoryPin(m: MemoryInfo) {
    const next = !m.pinned;
    await setMemoryPinned($activeProfileId, m.id, next);
    reloadMemory();
  }

  async function setSkillStatus(id: string, status: "approved" | "rejected") {
    await setSkillApproval(id, status);
    skillItems = skillItems.map((s) =>
      s.id === id ? { ...s, approval_status: status } : s,
    );
  }

  async function removeSkill(id: string) {
    // Two-click confirm, mirroring the memory/provider-remove pattern.
    if (confirmDeleteSkillId !== id) {
      confirmDeleteSkillId = id;
      setTimeout(() => {
        if (confirmDeleteSkillId === id) confirmDeleteSkillId = null;
      }, 3000);
      return;
    }
    confirmDeleteSkillId = null;
    await deleteSkill(id);
    skillItems = skillItems.filter((s) => s.id !== id);
    if (expandedSkillId === id) expandedSkillId = null;
  }

  function toggleSkillExpanded(id: string) {
    expandedSkillId = expandedSkillId === id ? null : id;
  }

  async function addSeat() {
    const name = seatName.trim();
    if (!name || !seatProviderId || !seatModel.trim()) {
      seatError = "A seat needs a name, a provider, and a model.";
      return;
    }
    seatSaving = true;
    seatError = null;
    try {
      await setSeatBinding($activeProfileId, name, seatProviderId, seatModel.trim());
      seatBindings = await listSeatBindings($activeProfileId);
      seatName = "";
      seatModel = "";
      seatProviderId = "";
    } catch (err) {
      seatError = String(err);
    } finally {
      seatSaving = false;
    }
  }

  async function unbindSeat(seat: string) {
    // Two-click confirm, mirroring the memory/skill-delete pattern.
    if (confirmUnbindSeat !== seat) {
      confirmUnbindSeat = seat;
      setTimeout(() => {
        if (confirmUnbindSeat === seat) confirmUnbindSeat = null;
      }, 3000);
      return;
    }
    confirmUnbindSeat = null;
    await deleteSeatBinding($activeProfileId, seat);
    seatBindings = seatBindings.filter((b) => b.seat !== seat);
  }

  function providerName(id: string): string {
    return providersStore.providers.find((p) => p.id === id)?.name ?? id;
  }

  async function doInstallPack() {
    if (!packJson.trim()) return;
    packInstalling = true;
    packNote = null;
    try {
      const r = await installPack($activeProfileId, packJson);
      packNote = `Installed “${r.pack_name}”: ${r.skills_installed} skill(s), ${r.agent_types_installed} agent type(s), ${r.cron_jobs_installed} cron job(s). Review and approve them below (and in Skills).`;
      packJson = "";
      agentTypes = await listAgentTypes();
    } catch (err) {
      packNote = `Couldn't install: ${String(err)}`;
    } finally {
      packInstalling = false;
    }
  }

  async function setAgentStatus(id: string, status: "approved" | "rejected") {
    await setAgentTypeApproval(id, status);
    agentTypes = agentTypes.map((a) => (a.id === id ? { ...a, approval_status: status } : a));
  }

  async function removeAgentType(id: string) {
    if (confirmDeleteAgentId !== id) {
      confirmDeleteAgentId = id;
      setTimeout(() => {
        if (confirmDeleteAgentId === id) confirmDeleteAgentId = null;
      }, 3000);
      return;
    }
    confirmDeleteAgentId = null;
    await deleteAgentType(id);
    agentTypes = agentTypes.filter((a) => a.id !== id);
  }

  async function toggleSkillReflect(next: boolean) {
    skillReflectSaving = true;
    const prev = skillReflectEnabled;
    skillReflectEnabled = next;
    try {
      await setSkillReflectEnabled(next);
    } catch {
      skillReflectEnabled = prev; // revert on failure
    } finally {
      skillReflectSaving = false;
    }
  }

  function formatMemDate(epochSeconds: number): string {
    return new Date(epochSeconds * 1000).toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
    });
  }

  const navBtnBase =
    "w-full cursor-pointer rounded-[var(--r)] px-2 py-[7px] text-left text-[12.5px] transition";
  const label = "text-[11px] font-semibold uppercase tracking-[0.06em] text-text-3";
  const rowBetween =
    "flex items-center justify-between gap-[14px] border-b border-border py-3";
</script>

<!-- Blurred app backdrop -->
<div class="grid h-screen grid-cols-[260px_1fr_0]">
  <Sidebar activeConv="Reply to landlord" />
  <main class="flex min-h-0 min-w-0 flex-col">
    <div class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border py-0 pl-[18px] pr-[14px]">
      <div class="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap text-[13.5px] font-semibold">
        Reply to landlord
      </div>
      <div class="flex-1"></div>
    </div>
    <div class="flex-1 overflow-y-auto px-5 py-[26px]">
      <div class="mx-auto flex max-w-[700px] flex-col gap-5">
        <ChatMessage role="user">
          Help me write a firm but polite reply to my landlord about the broken heater.
        </ChatMessage>
        <ChatMessage role="assistant">
          <p>
            Here's a draft that's firm on the timeline without burning the
            relationship. Want it warmer, or a firm deadline added?
          </p>
        </ChatMessage>
      </div>
    </div>
    <AppStatusBar session="0:12" />
  </main>
</div>

<!-- Modal overlay -->
<div class="fixed inset-0 z-40">
  <button
    type="button"
    aria-label="Close settings"
    onclick={goMain}
    class="absolute inset-0 cursor-default border-0 bg-[rgba(0,0,0,0.35)] backdrop-blur-[7px]"
  ></button>
  <div class="pointer-events-none absolute inset-0 grid place-items-center">
    <div
      class="pointer-events-auto grid h-[min(72vh,660px)] w-[min(68vw,960px)] grid-cols-[190px_minmax(0,1fr)]
        overflow-hidden rounded-[12px] border border-border-strong bg-surface shadow-[var(--shadow-pop)]"
      style="--accent:{accent}"
    >
      <!-- Submenu nav -->
      <div class="flex flex-col gap-0.5 border-r border-border bg-sidebar px-2.5 py-[14px]">
        <div class="{label} px-2 pb-2">Settings</div>
        {#each SECTIONS as [id, text] (id)}
          <button
            type="button"
            onclick={() => (section = id)}
            class="{navBtnBase} {section === id
              ? 'bg-surface-hover font-semibold text-text'
              : 'bg-transparent font-medium text-text-2'}"
          >
            {text}
          </button>
        {/each}
      </div>

      <!-- Content pane -->
      <div class="flex h-full min-h-0 flex-col">
        <div class="flex flex-shrink-0 items-center gap-2.5 border-b border-border px-[18px] py-[13px]">
          <span class="text-[13.5px] font-semibold">{activeLabel}</span>
          <div class="flex-1"></div>
          <IconButton label="Close" onclick={goMain}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M6 6l12 12M18 6 6 18" />
            </svg>
          </IconButton>
        </div>

        <div class="flex-1 overflow-y-auto px-[18px] pb-7 pt-4">
          {#if section === "routing"}
            <SettingRow
              title="New chats start as"
              desc="Where new conversations run until you change them"
            >
              {#snippet control()}
                <SegmentedControl
                  options={[
                    { value: "auto", label: "Auto" },
                    { value: "public", label: "Public" },
                    { value: "private", label: "Private" },
                  ]}
                  value={defaultBinding}
                  onchange={(v) => (defaultBinding = v)}
                />
              {/snippet}
            </SettingRow>
            <SettingRow
              title="When unsure, keep it local"
              desc="If it's not clear whether something is private, stay on this Mac"
            >
              {#snippet control()}
                <Toggle checked={uncertainty} onchange={(v) => (uncertainty = v)} />
              {/snippet}
            </SettingRow>
            <!-- The real, per-profile "Send only the safe parts" toggle lives in
                 the Privacy guard tab (wired to setRedactionEnabled). No duplicate
                 mock here. -->
          {:else if section === "privacy"}
            <SettingRow
              title="Egress guard"
              desc="Classify every outbound step before it leaves this Mac"
              dotColor="var(--local)"
            >
              {#snippet control()}
                <Toggle checked={guard} onchange={(v) => (guard = v)} />
              {/snippet}
            </SettingRow>

            {#if classifierError}
              <div class="mt-1 text-[11.5px] text-blocked">{classifierError}</div>
            {/if}

            <SettingRow
              title="Detection strictness"
              desc="Higher is more paranoid — more borderline content stays on this Mac."
            >
              {#snippet control()}
                <div class="flex flex-shrink-0 items-center gap-2.5">
                  <input
                    type="range"
                    min="0"
                    max="100"
                    step="1"
                    value={classifierStrictness}
                    disabled={classifierLoading || classifierSaving}
                    aria-label="Detection strictness"
                    oninput={(e) =>
                      (classifierStrictness = parseInt(e.currentTarget.value, 10))}
                    onchange={saveClassifierSettings}
                    class="w-[160px]"
                    style="accent-color:var(--accent)"
                  />
                  <span
                    class="w-[30px] shrink-0 rounded-[var(--r-sm)] bg-surface-2 px-[7px] py-0.5 text-center text-[10px] font-semibold text-text-2"
                  >
                    {classifierStrictness}
                  </span>
                </div>
              {/snippet}
            </SettingRow>

            <SettingRow
              title="Uncertainty band"
              desc="How much borderline content is marked “uncertain” rather than “private” in the review panel. Both stay on this Mac."
            >
              {#snippet control()}
                <SegmentedControl
                  options={[
                    { value: "narrow", label: "Narrow" },
                    { value: "medium", label: "Medium" },
                    { value: "wide", label: "Wide" },
                  ]}
                  value={classifierBand}
                  onchange={(v) => {
                    classifierBand = v as "narrow" | "medium" | "wide";
                    saveClassifierSettings();
                  }}
                />
              {/snippet}
            </SettingRow>

            <SettingRow
              title="Send only the safe parts to the cloud"
              desc="When a message is private only because of specific details (an email, an SSN, a card number), black those out and send the rest to a cloud model — the details stay on this Mac and the reply is restored locally."
            >
              {#snippet control()}
                <Toggle checked={classifierRedaction} onchange={toggleRedaction} />
              {/snippet}
            </SettingRow>

            <div class="mt-1">
              <Button variant="ghost" onclick={resetClassifier} disabled={classifierSaving}>
                Reset to defaults
              </Button>
            </div>

            <div class="{label} pb-1 pt-[18px]">
              Hard-block categories — never leave, any binding, no override
            </div>
            <SettingRow title="Health information">
              {#snippet control()}<Toggle checked locked />{/snippet}
            </SettingRow>
            <SettingRow title="Credentials & secrets">
              {#snippet control()}<Toggle checked locked />{/snippet}
            </SettingRow>
            <SettingRow title="Financial details">
              {#snippet control()}<Toggle checked locked />{/snippet}
            </SettingRow>
            <SettingRow title="SSN">
              {#snippet control()}<Toggle checked locked />{/snippet}
            </SettingRow>
            <div class="mt-3">
              <Button variant="ghost">Add category…</Button>
            </div>
          {:else if section === "permissions"}
            <div class="{label} pb-2">
              Standing tool permissions — the "Always allow" grants you've made in this profile
            </div>
            {#if rulesLoading && toolRules.length === 0}
              <p
                class="rounded-[var(--r-lg)] border border-dashed border-border-strong px-3 py-6 text-center text-[12.5px] text-text-3"
              >
                Loading permissions…
              </p>
            {:else if rulesError}
              <p
                class="rounded-[var(--r-lg)] border border-dashed border-border-strong px-3 py-6 text-center text-[12.5px] text-blocked"
              >
                Couldn't load permissions: {rulesError}
              </p>
            {:else if toolRules.length > 0}
              {#each toolRules as rule (rule.id)}
                <SettingRow
                  title={rule.tool_name}
                  desc={rule.pattern === "*"
                    ? "Any input · always allowed"
                    : `${rule.pattern} · always allowed`}
                  dotColor="var(--accent)"
                >
                  {#snippet control()}
                    <div class="flex flex-shrink-0 items-center gap-2.5">
                      <span class="text-[11px] text-text-3">{formatRuleDate(rule.created_at)}</span>
                      <Button variant="ghost" onclick={() => revokeRule(rule.id)}>
                        {confirmRevokeId === rule.id ? "Confirm?" : "Revoke"}
                      </Button>
                    </div>
                  {/snippet}
                </SettingRow>
              {/each}
              <p class="px-0.5 pt-3 text-[11px] text-text-3">
                Revoking takes effect immediately — the next time that tool runs, Lost
                Harness asks again. Risky tools can never earn a standing grant, so they
                never appear here.
              </p>
            {:else}
              <p
                class="rounded-[var(--r-lg)] border border-dashed border-border-strong px-3 py-8 text-center text-[12.5px] text-text-3"
              >
                No standing permissions yet. When you choose “Always allow” on a tool
                request, it appears here so you can take it back.
              </p>
            {/if}
          {:else if section === "models"}
            {#if providersStore.loading && providersStore.providers.length === 0}
              <p
                class="rounded-[var(--r-lg)] border border-dashed border-border-strong px-3 py-6 text-center text-[12.5px] text-text-3"
              >
                Loading providers…
              </p>
            {:else if providersStore.providers.length > 0}
              {#each providersStore.providers as p (p.id)}
                {@const models = modelsFor(p)}
                {@const isActive = p.id === providersStore.activeProviderId}
                <SettingRow
                  title={p.name}
                  desc={`${p.kind === "local" ? "Local" : p.kind === "cloud" ? "Cloud" : "Custom"} · ${p.baseUrl}`}
                  dotColor={providerDotColor(p.kind)}
                  tag={isActive
                    ? { label: "active", bg: "var(--accent-soft)", color: "var(--accent)" }
                    : undefined}
                >
                  {#snippet control()}
                    <div class="flex flex-shrink-0 items-center gap-1.5">
                      <Select
                        items={models.map((m) => ({ value: m, label: m }))}
                        value={isActive ? (providersStore.activeModel ?? "") : ""}
                        onchange={(v) => setActiveModel(p.id, v)}
                        placeholder={models.length > 0 ? "Select model" : "No models"}
                        disabled={models.length === 0}
                      />
                      <Button variant="ghost" onclick={() => startEditProvider(p)}>
                        {providerForm?.id === p.id ? "Cancel" : "Edit"}
                      </Button>
                      <Button variant="ghost" onclick={() => handleRemoveProvider(p.id)}>
                        {confirmRemoveProviderId === p.id ? "Confirm?" : "Remove"}
                      </Button>
                    </div>
                  {/snippet}
                </SettingRow>
              {/each}
            {:else}
              <p
                class="rounded-[var(--r-lg)] border border-dashed border-border-strong px-3 py-6 text-center text-[12.5px] text-text-3"
              >
                No providers configured yet. Add one below.
              </p>
            {/if}

            <div class="mt-4">
              <div class="{label} pb-1.5">Quick add</div>
              <div class="flex flex-wrap gap-1.5">
                {#each QUICK_PROVIDER_PRESETS as preset (preset.id)}
                  <button
                    type="button"
                    onclick={() => startAddProviderFromPreset(preset)}
                    class="cursor-pointer rounded-[var(--r)] border border-border bg-surface px-2.5 py-1 text-[11.5px] font-medium
                      text-text-2 transition hover:border-border-strong hover:bg-surface-hover hover:text-text"
                  >
                    {preset.name}
                  </button>
                {/each}
              </div>
            </div>

            {#if providerForm}
              <form
                class="mt-3 space-y-2.5 rounded-[var(--r-lg)] border border-border bg-surface px-[13px] py-[13px]"
                onsubmit={(e) => {
                  e.preventDefault();
                  saveProvider();
                }}
              >
                <div>
                  <label class="{label} mb-1 block" for="prov-name">Name</label>
                  <input
                    id="prov-name"
                    type="text"
                    class="w-full rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12.5px]
                      text-text outline-none placeholder:text-text-3 focus:border-accent"
                    placeholder="My OpenAI"
                    value={providerForm.name}
                    oninput={(e) =>
                      (providerForm = {
                        ...providerForm!,
                        name: (e.currentTarget as HTMLInputElement).value,
                      })}
                  />
                </div>
                <div>
                  <label class="{label} mb-1 block" for="prov-url">Base URL</label>
                  <input
                    id="prov-url"
                    type="text"
                    class="w-full rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12.5px]
                      text-text outline-none placeholder:text-text-3 focus:border-accent"
                    placeholder="https://api.openai.com/v1"
                    value={providerForm.baseUrl}
                    oninput={(e) =>
                      (providerForm = {
                        ...providerForm!,
                        baseUrl: (e.currentTarget as HTMLInputElement).value,
                      })}
                  />
                </div>
                <div>
                  <label class="{label} mb-1 block" for="prov-key">API key</label>
                  <div class="flex gap-1.5">
                    <input
                      id="prov-key"
                      type={showProviderKey ? "text" : "password"}
                      class="min-w-0 flex-1 rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12.5px]
                        text-text outline-none placeholder:text-text-3 focus:border-accent"
                      placeholder={providerForm.id ? "••• saved — leave blank to keep" : "sk-…"}
                      value={providerForm.apiKey}
                      oninput={(e) =>
                        (providerForm = {
                          ...providerForm!,
                          apiKey: (e.currentTarget as HTMLInputElement).value,
                        })}
                    />
                    <Button variant="ghost" onclick={() => (showProviderKey = !showProviderKey)}>
                      {showProviderKey ? "Hide" : "Show"}
                    </Button>
                  </div>
                </div>
                <div>
                  <div class="{label} mb-1">Kind</div>
                  <Select
                    items={PROVIDER_KIND_OPTIONS}
                    value={providerForm.kind}
                    onchange={(v) =>
                      (providerForm = { ...providerForm!, kind: v as ProviderKind })}
                  />
                </div>
                <div>
                  <div class="flex items-center justify-between gap-2">
                    <span class="{label}">Native tool-calling</span>
                    <Toggle
                      checked={providerForm.supportsNativeTools}
                      onchange={(v) =>
                        (providerForm = { ...providerForm!, supportsNativeTools: v })}
                      label="This endpoint supports native tool-calling (OpenAI-style tool_calls)"
                    />
                  </div>
                  <p class="mt-1 text-[11px] leading-[1.4] text-text-3">
                    This endpoint supports native tool-calling (OpenAI-style tool_calls). When
                    unchecked, Lost Harness falls back to the fenced tool-call format for this
                    provider.
                  </p>
                </div>
                <div class="flex items-center justify-end gap-2 pt-1">
                  <Button variant="ghost" onclick={cancelProviderForm}>Cancel</Button>
                  <Button variant="primary" type="submit" disabled={!canSaveProvider || savingProvider}>
                    {savingProvider ? "Saving…" : providerForm.id ? "Save changes" : "Add provider"}
                  </Button>
                </div>
              </form>
            {:else}
              <button
                type="button"
                onclick={startAddProvider}
                class="mt-3 flex w-full cursor-pointer items-center gap-[11px] rounded-[var(--r-lg)] border border-dashed
                  border-border-strong bg-transparent px-[13px] py-[11px] text-left text-text-2"
              >
                <span class="grid h-[22px] w-[22px] flex-shrink-0 place-items-center rounded-full bg-surface-2">
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <path d="M12 5v14M5 12h14" />
                  </svg>
                </span>
                <span class="min-w-0">
                  <span class="block text-[12.5px] font-[550] text-text">Add an endpoint</span>
                  <span class="block text-[11.5px] text-text-3">
                    OpenAI-compatible or Ollama — models are discovered automatically
                  </span>
                </span>
              </button>
            {/if}

            <!-- Downloadable local models (Wave 5.3 / M8), sized to this machine -->
            <div class="mb-2 mt-6 flex items-center gap-2.5">
              <span class="text-[12px] font-[550] text-text">Download a local model</span>
              {#if hardware}
                <span class="text-[11.5px] text-text-3">
                  {fmtGB(hardware.total_ram_bytes)} RAM · {hardware.cpu_cores} cores · sized to your machine
                </span>
              {/if}
            </div>
            <div class="flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border" data-testid="model-catalog">
              {#if catalogModels.length === 0}
                <div class="px-3 py-6 text-center text-[12px] text-text-3">
                  No catalog models available.
                </div>
              {:else}
                {#each catalogModels as m (m.id)}
                  <div class="flex items-center gap-2 border-b border-border py-[9px] pl-3 pr-2.5" data-testid="catalog-row">
                    <span class="min-w-0 flex-1">
                      <span class="block truncate text-[12.5px] font-[550] text-text">{m.name}</span>
                      <span class="block truncate text-[11.5px] text-text-3">{m.description} · {fmtGB(m.size_bytes)}</span>
                    </span>
                    {#if m.fit === "fits"}
                      <span class="flex-shrink-0 rounded-[8px] bg-local-soft px-[7px] py-px text-[10px] text-local">fits</span>
                    {:else if m.fit === "tight"}
                      <span class="flex-shrink-0 rounded-[8px] bg-warn-soft px-[7px] py-px text-[10px] text-warn">tight</span>
                    {:else}
                      <span class="flex-shrink-0 rounded-[8px] bg-surface-2 px-[7px] py-px text-[10px] text-text-3">too large</span>
                    {/if}
                    {#if modelDownloadStatus[m.id]}
                      <span class="flex-shrink-0 text-[11px] text-text-2">{modelDownloadStatus[m.id]}</span>
                    {:else}
                      <Button
                        variant="ghost"
                        disabled={!m.installable || m.fit === "too_large"}
                        onclick={() => startModelDownload(m)}
                      >
                        {m.installable ? "Download" : "Soon"}
                      </Button>
                    {/if}
                  </div>
                {/each}
              {/if}
              <div class="px-3 py-2 text-[11px] text-text-3">
                Models download from Hugging Face and are verified by checksum before use — a
                mismatch installs nothing. (Entries marked “Soon” await release curation of their
                verified hash.)
              </div>
            </div>

            {#if localModels.length > 0}
              <div class="mb-2 mt-4 flex items-center gap-2.5">
                <span class="text-[12px] font-[550] text-text">Downloaded models</span>
              </div>
              <div class="flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border" data-testid="local-models">
                {#each localModels as lm (lm.id)}
                  <div class="flex items-center gap-2 border-b border-border py-[9px] pl-3 pr-2.5">
                    <span class="min-w-0 flex-1">
                      <span class="block truncate text-[12.5px] font-[550] text-text">{lm.name}</span>
                      <span class="block truncate text-[11.5px] text-text-3">{fmtGB(lm.size_bytes)}</span>
                    </span>
                    {#if lm.status === "quarantined"}
                      <span class="flex-shrink-0 rounded-[8px] bg-blocked-soft px-[7px] py-px text-[10px] text-blocked">quarantined</span>
                    {:else}
                      <span class="flex-shrink-0 rounded-[8px] bg-local-soft px-[7px] py-px text-[10px] text-local">ready</span>
                    {/if}
                    <Button variant="ghost" onclick={() => removeModel(lm.id)}>
                      {confirmRemoveModelId === lm.id ? "Confirm?" : "Remove"}
                    </Button>
                  </div>
                {/each}
              </div>
            {/if}

            <!-- Seats (Wave 3.1): named roles → a model, per profile -->
            <div class="mb-2 mt-6 flex items-center gap-2.5">
              <span class="text-[12px] font-[550] text-text">Seats</span>
              <span class="text-[11.5px] text-text-3">
                Name a role (e.g. “Coding”), point it at a model — this profile only
              </span>
            </div>
            {#if seatError}
              <div class="mb-2 text-[11.5px] text-blocked" data-testid="seat-error">{seatError}</div>
            {/if}
            <div class="flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border" data-testid="seats-list">
              {#if seatBindings.length === 0}
                <div class="px-3 py-6 text-center text-[12px] text-text-3">
                  No seats yet. Unbound seats fall back to the model the conversation is already using.
                </div>
              {:else}
                {#each seatBindings as b (b.seat)}
                  <div class="flex items-center gap-2 border-b border-border py-[9px] pl-3 pr-2.5" data-testid="seat-row">
                    <span class="min-w-0 flex-1">
                      <span class="block text-[12.5px] font-[550] text-text">{b.seat}</span>
                      <span class="block truncate text-[11.5px] text-text-3">{providerName(b.provider_id)} · {b.model}</span>
                    </span>
                    <button
                      type="button"
                      aria-label="Unbind this seat"
                      title={confirmUnbindSeat === b.seat ? "Click again to unbind" : "Unbind"}
                      onclick={() => unbindSeat(b.seat)}
                      class="grid h-6 w-6 flex-shrink-0 place-items-center rounded-[var(--r)] border-0 bg-transparent
                        {confirmUnbindSeat === b.seat ? 'text-blocked' : 'text-text-3'} hover:bg-surface-hover hover:text-text"
                    >
                      <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                        <path d="M6 6l12 12M18 6 6 18" />
                      </svg>
                    </button>
                  </div>
                {/each}
              {/if}
              <form
                class="flex flex-wrap items-center gap-1.5 px-2.5 py-2"
                onsubmit={(e) => {
                  e.preventDefault();
                  addSeat();
                }}
              >
                <input
                  bind:value={seatName}
                  placeholder="Seat name"
                  class="min-w-[100px] flex-1 rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12.5px]
                    text-text outline-none placeholder:text-text-3 focus:border-accent"
                />
                <select
                  bind:value={seatProviderId}
                  class="rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12.5px] text-text outline-none focus:border-accent"
                >
                  <option value="" disabled>Provider…</option>
                  {#each providersStore.providers as p (p.id)}
                    <option value={p.id}>{p.name}</option>
                  {/each}
                </select>
                <input
                  bind:value={seatModel}
                  placeholder="Model"
                  class="min-w-[100px] flex-1 rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12.5px]
                    text-text outline-none placeholder:text-text-3 focus:border-accent"
                />
                <Button variant="primary" type="submit" disabled={seatSaving}>
                  {seatSaving ? "Saving…" : "Bind"}
                </Button>
              </form>
              <div class="px-3 py-2 text-[11px] text-text-3">
                An agent that asks for a seat gets its bound model — rebind here to change
                that with no code change. A seat can prefer a cloud model, but a
                must-stay-local turn is still routed to a local one by the privacy guard.
              </div>
            </div>
          {:else if section === "memory"}
            {#if memSettingsError}
              <div class="mt-1 text-[11.5px] text-blocked">{memSettingsError}</div>
            {/if}
            <SettingRow
              title="Memory privacy"
              desc="Shared = one memory across profiles · Walled = each profile keeps its own private store"
            >
              {#snippet control()}
                <SegmentedControl
                  options={[
                    { value: "shared", label: "Shared" },
                    { value: "walled", label: "Walled" },
                  ]}
                  value={memoryMode}
                  onchange={setMemoryModeAndSave}
                />
              {/snippet}
            </SettingRow>
            {#if memoryMode === "walled"}
              <p class="px-0.5 pb-1 text-[11px] text-text-3">
                Walled keeps this profile's memory in its own on-device store — physically
                separate from other profiles, not just filtered.
              </p>
            {/if}
            <SettingRow
              title="Semantic memory search"
              desc="Off = keyword-only search; no meaning fingerprint is computed for your saved notes."
            >
              {#snippet control()}
                <Toggle
                  checked={semanticSearchEnabled}
                  locked={memSettingsSaving}
                  onchange={toggleSemanticSearch}
                  label="Semantic memory search"
                />
              {/snippet}
            </SettingRow>
            <div class="mb-2 mt-4 flex items-center gap-2.5">
              <span class="text-[12px] font-[550] text-text">This profile's memory</span>
              <span class="text-[11.5px] text-text-3">
                {memoryItems.length}
                {memoryItems.length === 1 ? "fact" : "facts"} · stored on this Mac
              </span>
              <div class="flex-1"></div>
            </div>

            <form
              class="mb-2 flex items-center gap-1.5"
              onsubmit={(e) => {
                e.preventDefault();
                addMemoryFact();
              }}
            >
              <input
                bind:value={memDraft}
                placeholder="Add something to remember…"
                class="min-w-0 flex-1 rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12.5px]
                  text-text outline-none placeholder:text-text-3 focus:border-accent"
              />
              <Button variant="primary" type="submit" disabled={!memDraft.trim() || memSaving}>
                {memSaving ? "Saving…" : "Remember"}
              </Button>
            </form>
            {#if memNote}
              <p class="mb-2 text-[11.5px] text-text-2">{memNote}</p>
            {/if}

            <div class="flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border">
              {#if memoryLoading && memoryItems.length === 0}
                <div class="px-3 py-6 text-center text-[12px] text-text-3">Loading…</div>
              {:else if memoryItems.length === 0}
                <div class="px-3 py-8 text-center text-[12px] text-text-3">
                  No memories yet. Anything you add — or the assistant saves — appears
                  here, and you can forget it any time.
                </div>
              {:else}
                {#each memoryItems as m (m.id)}
                  <div class="flex items-center gap-2 border-b border-border py-[7px] pl-2.5 pr-2.5">
                    <button
                      type="button"
                      aria-label={m.pinned ? "Unpin from summary" : "Pin to summary"}
                      title={m.pinned
                        ? "Pinned into every conversation — click to unpin"
                        : "Pin into the always-loaded summary"}
                      onclick={() => toggleMemoryPin(m)}
                      class="grid h-6 w-6 flex-shrink-0 place-items-center rounded-[var(--r)] border-0 bg-transparent
                        {m.pinned ? 'text-accent' : 'text-text-3'} hover:bg-surface-hover"
                    >
                      <svg width="12" height="12" viewBox="0 0 24 24" fill={m.pinned ? "currentColor" : "none"} stroke="currentColor" stroke-width="1.6">
                        <path d="M12 2l2.9 6.3L22 9.3l-5 4.6 1.3 6.8L12 17.6 5.7 20.7 7 13.9l-5-4.6 7.1-1z" />
                      </svg>
                    </button>
                    <span class="min-w-0 flex-1 text-[12.5px] text-text">{m.content}</span>
                    {#if m.sensitivity === "private_local"}
                      <span
                        class="flex-shrink-0 rounded-[8px] bg-blocked-soft px-[7px] py-px text-[10px] text-blocked"
                      >
                        on device only
                      </span>
                    {/if}
                    <span class="flex-shrink-0 text-[11px] text-text-3">{formatMemDate(m.created_at)}</span>
                    <button
                      type="button"
                      aria-label="Forget this memory"
                      title={confirmForgetId === m.id ? "Click again to forget" : "Forget"}
                      onclick={() => forgetMemory(m.id)}
                      class="grid h-6 w-6 flex-shrink-0 place-items-center rounded-[var(--r)] border-0 bg-transparent
                        {confirmForgetId === m.id ? 'text-blocked' : 'text-text-3'} hover:bg-surface-hover hover:text-text"
                    >
                      <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                        <path d="M6 6l12 12M18 6 6 18" />
                      </svg>
                    </button>
                  </div>
                {/each}
              {/if}
              <div class="px-3 py-2 text-[11px] text-text-3">
                Facts are routed by sensitivity: secrets are never saved, private
                details stay on this Mac, and pinned facts load into every conversation.
              </div>
            </div>
          {:else if section === "skills"}
            {#if skillsError}
              <div class="mb-2 text-[11.5px] text-blocked" data-testid="skills-error">{skillsError}</div>
            {/if}
            <SettingRow
              title="Propose skills automatically"
              desc="After a chat, a local model may draft a reusable skill from it. Drafts stay on this Mac and are inert until you approve them below — never used without your review."
            >
              {#snippet control()}
                <Toggle
                  checked={skillReflectEnabled}
                  locked={skillReflectSaving}
                  onchange={toggleSkillReflect}
                  label="Propose skills automatically"
                />
              {/snippet}
            </SettingRow>
            <div class="mb-2 mt-4 flex items-center gap-2.5">
              <span class="text-[12px] font-[550] text-text">Saved skills</span>
              <span class="text-[11.5px] text-text-3">
                {skillItems.length}
                {skillItems.length === 1 ? "skill" : "skills"} · shared across profiles
              </span>
              <div class="flex-1"></div>
            </div>

            <div class="flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border" data-testid="skills-list">
              {#if skillsLoading && skillItems.length === 0}
                <div class="px-3 py-6 text-center text-[12px] text-text-3">Loading…</div>
              {:else if skillItems.length === 0}
                <div class="px-3 py-8 text-center text-[12px] text-text-3">
                  No skills yet. When the assistant saves a reusable routine — with your
                  one-time approval — it appears here for you to review, revoke, or delete.
                </div>
              {:else}
                {#each skillItems as s (s.id)}
                  <div class="flex flex-col gap-1.5 border-b border-border py-[9px] pl-2.5 pr-2.5" data-testid="skill-row">
                    <div class="flex items-center gap-2">
                      <button
                        type="button"
                        aria-label={expandedSkillId === s.id ? "Hide skill body" : "Show skill body"}
                        title={expandedSkillId === s.id ? "Hide what this skill does" : "Show what this skill does"}
                        onclick={() => toggleSkillExpanded(s.id)}
                        class="grid h-6 w-6 flex-shrink-0 place-items-center rounded-[var(--r)] border-0 bg-transparent
                          text-text-3 transition hover:bg-surface-hover hover:text-text"
                      >
                        <svg
                          width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
                          style="transform: rotate({expandedSkillId === s.id ? 90 : 0}deg); transition: transform .12s"
                        >
                          <path d="M9 6l6 6-6 6" />
                        </svg>
                      </button>
                      <div class="min-w-0 flex-1">
                        <div class="truncate text-[12.5px] font-[550] text-text">{s.name}</div>
                        <div class="truncate text-[11.5px] text-text-3">{s.description}</div>
                      </div>
                      {#if s.approval_status === "approved"}
                        <span class="flex-shrink-0 rounded-[8px] bg-local-soft px-[7px] py-px text-[10px] text-local">approved</span>
                      {:else if s.approval_status === "pending"}
                        <span class="flex-shrink-0 rounded-[8px] bg-warn-soft px-[7px] py-px text-[10px] text-warn">pending review</span>
                      {:else}
                        <span class="flex-shrink-0 rounded-[8px] bg-surface-2 px-[7px] py-px text-[10px] text-text-3">rejected</span>
                      {/if}
                      <button
                        type="button"
                        aria-label="Delete this skill"
                        title={confirmDeleteSkillId === s.id ? "Click again to delete" : "Delete"}
                        onclick={() => removeSkill(s.id)}
                        class="grid h-6 w-6 flex-shrink-0 place-items-center rounded-[var(--r)] border-0 bg-transparent
                          {confirmDeleteSkillId === s.id ? 'text-blocked' : 'text-text-3'} hover:bg-surface-hover hover:text-text"
                      >
                        <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                          <path d="M6 6l12 12M18 6 6 18" />
                        </svg>
                      </button>
                    </div>
                    {#if s.capabilities_required.length > 0}
                      <div class="flex flex-wrap gap-1 pl-8">
                        {#each s.capabilities_required as cap (cap)}
                          <span class="rounded-[6px] bg-surface-2 px-[6px] py-px text-[10px] text-text-3">{cap}</span>
                        {/each}
                      </div>
                    {/if}
                    {#if expandedSkillId === s.id}
                      <pre class="ml-8 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-[var(--r)] border border-border bg-surface-2 px-2.5 py-2 text-[11.5px] text-text-2">{s.content}</pre>
                      <div class="flex items-center gap-1.5 pl-8">
                        {#if s.approval_status !== "approved"}
                          <Button variant="primary" onclick={() => setSkillStatus(s.id, "approved")}>Approve</Button>
                        {/if}
                        {#if s.approval_status !== "rejected"}
                          <Button variant="ghost" onclick={() => setSkillStatus(s.id, "rejected")}>Reject</Button>
                        {/if}
                      </div>
                    {/if}
                  </div>
                {/each}
              {/if}
              <div class="px-3 py-2 text-[11px] text-text-3">
                A skill is a saved routine the assistant can reuse. Only <span class="text-text-2">approved</span>
                skills are searchable and usable; saving one always requires your explicit
                one-time approval, and its actions are still gated like any other tool.
              </div>
            </div>
          {:else if section === "agents"}
            {#if agentsError}
              <div class="mb-2 text-[11.5px] text-blocked" data-testid="agents-error">{agentsError}</div>
            {/if}
            <!-- Capability packs (Wave 4.5): install a bundle of skills + agents + cron -->
            <SettingRow
              title="Install a capability pack"
              desc="A shareable bundle of skills, agent types, and cron jobs. Everything lands for review — nothing is armed until you approve it."
            >
              {#snippet control()}
                <Button variant="ghost" onclick={() => (packOpen = !packOpen)}>
                  {packOpen ? "Close" : "Install…"}
                </Button>
              {/snippet}
            </SettingRow>
            {#if packOpen}
              <div class="mb-3 flex flex-col gap-1.5 rounded-[var(--r-lg)] border border-border p-2.5">
                <textarea
                  bind:value={packJson}
                  placeholder="Paste a capability-pack JSON here…"
                  rows="4"
                  class="w-full resize-y rounded-[var(--r)] border border-border bg-surface-2 px-[9px] py-[6px] font-mono text-[11.5px]
                    text-text outline-none placeholder:text-text-3 focus:border-accent"
                ></textarea>
                <div class="flex items-center gap-2">
                  <Button variant="primary" onclick={doInstallPack} disabled={!packJson.trim() || packInstalling}>
                    {packInstalling ? "Installing…" : "Install pack"}
                  </Button>
                  {#if packNote}
                    <span class="text-[11px] text-text-2">{packNote}</span>
                  {/if}
                </div>
              </div>
            {/if}
            <div class="mb-2 mt-4 flex items-center gap-2.5">
              <span class="text-[12px] font-[550] text-text">Agent types</span>
              <span class="text-[11.5px] text-text-3">
                Named helper personas the assistant can delegate to — bounded toolbelt + a model seat
              </span>
              <div class="flex-1"></div>
            </div>
            <div class="flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border" data-testid="agents-list">
              {#if agentsLoading && agentTypes.length === 0}
                <div class="px-3 py-6 text-center text-[12px] text-text-3">Loading…</div>
              {:else if agentTypes.length === 0}
                <div class="px-3 py-8 text-center text-[12px] text-text-3">
                  No agent types yet.
                </div>
              {:else}
                {#each agentTypes as a (a.id)}
                  <div class="flex flex-col gap-1.5 border-b border-border py-[9px] pl-3 pr-2.5" data-testid="agent-row">
                    <div class="flex items-center gap-2">
                      <div class="min-w-0 flex-1">
                        <div class="truncate text-[12.5px] font-[550] text-text">{a.name}</div>
                        <div class="truncate text-[11.5px] text-text-3">{a.description}</div>
                      </div>
                      {#if a.approval_status === "approved"}
                        <span class="flex-shrink-0 rounded-[8px] bg-local-soft px-[7px] py-px text-[10px] text-local">approved</span>
                      {:else if a.approval_status === "pending"}
                        <span class="flex-shrink-0 rounded-[8px] bg-warn-soft px-[7px] py-px text-[10px] text-warn">pending review</span>
                      {:else}
                        <span class="flex-shrink-0 rounded-[8px] bg-surface-2 px-[7px] py-px text-[10px] text-text-3">rejected</span>
                      {/if}
                      {#if a.source === "builtin"}
                        <span class="flex-shrink-0 rounded-[8px] bg-surface-2 px-[7px] py-px text-[10px] text-text-3">built-in</span>
                      {/if}
                      <button
                        type="button"
                        aria-label="Delete this agent type"
                        title={confirmDeleteAgentId === a.id ? "Click again to delete" : "Delete"}
                        onclick={() => removeAgentType(a.id)}
                        class="grid h-6 w-6 flex-shrink-0 place-items-center rounded-[var(--r)] border-0 bg-transparent
                          {confirmDeleteAgentId === a.id ? 'text-blocked' : 'text-text-3'} hover:bg-surface-hover hover:text-text"
                      >
                        <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                          <path d="M6 6l12 12M18 6 6 18" />
                        </svg>
                      </button>
                    </div>
                    <div class="flex flex-wrap items-center gap-1 pl-0">
                      <span class="rounded-[6px] bg-surface-2 px-[6px] py-px text-[10px] text-text-3">seat: {a.seat}</span>
                      {#each a.tools_allowlist as tool (tool)}
                        <span class="rounded-[6px] bg-surface-2 px-[6px] py-px text-[10px] text-text-3">{tool}</span>
                      {/each}
                    </div>
                    <div class="flex items-center gap-1.5">
                      {#if a.approval_status !== "approved"}
                        <Button variant="primary" onclick={() => setAgentStatus(a.id, "approved")}>Approve</Button>
                      {/if}
                      {#if a.approval_status !== "rejected"}
                        <Button variant="ghost" onclick={() => setAgentStatus(a.id, "rejected")}>Reject</Button>
                      {/if}
                    </div>
                  </div>
                {/each}
              {/if}
              <div class="px-3 py-2 text-[11px] text-text-3">
                Only <span class="text-text-2">approved</span> agent types can be dispatched via
                the <span class="text-text-2">delegate</span> tool. A helper runs with only its
                listed tools (intersected with what's available), each call still gated, and its
                seat's model — inheriting the conversation's privacy binding.
              </div>
            </div>
          {:else if section === "usage"}
            {#if usageLoading}
              <div class="px-3 py-4 text-sm text-text-3">Loading usage…</div>
            {:else if usageError}
              <div class="px-3 py-4 text-sm text-red-400" data-testid="usage-error">{usageError}</div>
            {:else if usage}
              <div class="flex flex-col gap-2" data-testid="usage-summary">
                <SettingRow title="Model calls" desc="Total model calls booked for this profile">
                  {#snippet control()}
                    <span class="text-sm tabular-nums text-text-1">{usage?.total_calls ?? 0}</span>
                  {/snippet}
                </SettingRow>
                <SettingRow title="Known cost" desc="Local calls are $0; priced cloud calls are summed">
                  {#snippet control()}
                    <span class="text-sm tabular-nums text-text-1"
                      >${(usage?.known_cost_usd ?? 0).toFixed(4)}</span>
                  {/snippet}
                </SettingRow>
                <SettingRow
                  title="Unpriced cloud calls"
                  desc="Cloud calls we couldn't price — shown honestly, never guessed as $0"
                >
                  {#snippet control()}
                    <span
                      class="text-sm tabular-nums {(usage?.unknown_cost_calls ?? 0) > 0
                        ? 'text-amber-400'
                        : 'text-text-1'}">{usage?.unknown_cost_calls ?? 0}</span>
                  {/snippet}
                </SettingRow>
                <div class="px-3 py-2 text-[11px] text-text-3">
                  Cost is captured from the endpoint's reported token usage and a
                  built-in price list for well-known cloud models. A call is only
                  priced when both are available — otherwise it's counted as
                  “unpriced,” never guessed.
                </div>
              </div>
            {:else}
              <div class="px-3 py-4 text-sm text-text-3">No usage recorded yet.</div>
            {/if}
          {:else if section === "appearance"}
            <SettingRow title="Theme" desc="Follow the system, or pick one">
              {#snippet control()}
                <SegmentedControl
                  options={[
                    { value: "dark", label: "Dark" },
                    { value: "light", label: "Light" },
                    { value: "system", label: "System" },
                  ]}
                  value={$theme}
                  onchange={setTheme}
                />
              {/snippet}
            </SettingRow>

            <div class={rowBetween}>
              <div class="min-w-0">
                <div class="text-[13px] font-[550]">Accent</div>
                <div class="text-[11.5px] text-text-3">
                  Selection, focus, and primary actions — pick a swatch or type any hex
                </div>
              </div>
              <div class="flex flex-shrink-0 items-center gap-[7px]">
                {#each ACCENTS as c (c)}
                  <button
                    type="button"
                    aria-label={`Accent ${c}`}
                    onclick={() => (accent = c)}
                    class="h-5 w-5 cursor-pointer rounded-full border-2 border-transparent p-0 outline outline-1 outline-[var(--border-strong)]"
                    style="background:{c}"
                  ></button>
                {/each}
                <input
                  value={accent}
                  oninput={(e) => (accent = e.currentTarget.value)}
                  aria-label="Custom accent hex"
                  class="w-[78px] rounded-[var(--r)] border border-border bg-surface-2 px-2 py-[5px] font-mono text-[11.5px] text-text"
                />
              </div>
            </div>

            <SettingRow title="Background tone" desc="Subtle tint under the neutral surfaces">
              {#snippet control()}
                <SegmentedControl
                  options={[
                    { value: "neutral", label: "Neutral" },
                    { value: "warm", label: "Warm" },
                    { value: "cool", label: "Cool" },
                  ]}
                  value={tone}
                  onchange={(v) => (tone = v)}
                />
              {/snippet}
            </SettingRow>
            <SettingRow title="Density" desc="Spacing of lists, threads, and controls">
              {#snippet control()}
                <SegmentedControl
                  options={[
                    { value: "compact", label: "Compact" },
                    { value: "cozy", label: "Cozy" },
                    { value: "comfortable", label: "Comfortable" },
                  ]}
                  value={density}
                  onchange={(v) => (density = v)}
                />
              {/snippet}
            </SettingRow>

            <div class={rowBetween}>
              <div>
                <div class="text-[13px] font-[550]">Text size</div>
                <div class="text-[11.5px] text-text-3">Applies everywhere, including the editor</div>
              </div>
              <div class="flex flex-shrink-0 items-center gap-2.5">
                <input
                  type="range"
                  min="12"
                  max="18"
                  step="0.5"
                  value={fontSize}
                  oninput={(e) => (fontSize = parseFloat(e.currentTarget.value))}
                  class="w-[140px]"
                  style="accent-color:var(--accent)"
                />
                <span class="w-[38px] text-[11.5px] text-text-2">{fontSize}px</span>
              </div>
            </div>

            <SettingRow title="Reduce motion" desc="Disable panel and thread animations">
              {#snippet control()}
                <Toggle checked={motion} onchange={(v) => (motion = v)} />
              {/snippet}
            </SettingRow>
          {/if}
        </div>
      </div>
    </div>
  </div>
</div>
