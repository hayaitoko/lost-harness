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
  import SettingRow from "../components/SettingRow.svelte";
  import Toggle from "../components/Toggle.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";

  type Section = "routing" | "privacy" | "models" | "memory" | "appearance";
  const SECTIONS: [Section, string][] = [
    ["routing", "Routing"],
    ["privacy", "Privacy guard"],
    ["models", "Models"],
    ["memory", "Memory"],
    ["appearance", "Appearance"],
  ];

  type Memory = { text: string; date: string; tag?: string };
  const INITIAL_MEMORIES: Record<string, Memory[]> = {
    Personal: [
      { text: "Prefers concise, direct replies", date: "Jun 12" },
      { text: "Home address: 123 Oak Street, Apt 4B", date: "Jul 2", tag: "never leaves" },
      { text: "Lease renews in August; landlord is Marcus Webb", date: "Jul 8" },
      { text: "Planning a Kyoto trip Oct 12–16 with Nina", date: "Jul 10" },
      { text: "Vitamin D slightly low — flagged by Dr. Chen", date: "Jul 8", tag: "health · hard-blocked" },
    ],
    Work: [
      { text: "Works in Rust on the payments service", date: "Jun 20" },
      { text: "Weekly report due Fridays to Priya", date: "Jun 27" },
      { text: "Prefers PR descriptions in bullet form", date: "Jul 3" },
    ],
  };

  const ACCENTS = ["#5f74e0", "#3fa87d", "#4a97cf", "#c49a55", "#b06fc2", "#d0685f", "#5fb8b0", "#8a8a93"];

  const goMain = () => nav.go("main");
  let section = $state<Section>("routing");

  // routing
  let defaultBinding = $state("auto");
  let uncertainty = $state(true);
  let redaction = $state(false);
  // privacy
  let guard = $state(true);
  // memory
  let memoryMode = $state("walled");
  let memProfile = $state("Personal");
  let memMenuOpen = $state(false);
  let memories = $state<Record<string, Memory[]>>(INITIAL_MEMORIES);
  let editingMem = $state<number | null>(null);
  let cancelEdit = false;
  // appearance
  let themeSel = $state("dark");
  let accent = $state("#5f74e0");
  let tone = $state("neutral");
  let density = $state("cozy");
  let fontSize = $state(13.5);
  let motion = $state(true);

  let mems = $derived(memories[memProfile] ?? []);
  let activeLabel = $derived(SECTIONS.find(([id]) => id === section)![1]);

  function updateMems(fn: (arr: Memory[]) => Memory[]) {
    memories = { ...memories, [memProfile]: fn(memories[memProfile] ?? []) };
  }

  function addMemory() {
    const i = mems.length;
    updateMems((arr) => [...arr, { text: "New memory", date: "now" }]);
    editingMem = i;
  }
  function commitMem(i: number, value: string) {
    if (cancelEdit) {
      cancelEdit = false;
      editingMem = null;
      return;
    }
    const v = value.trim();
    updateMems((arr) => {
      const next = arr.slice();
      if (v) next[i] = { ...next[i], text: v };
      else next.splice(i, 1);
      return next;
    });
    editingMem = null;
  }
  function applyTheme(v: string) {
    themeSel = v;
    document.documentElement.dataset.theme = v === "light" ? "light" : "dark";
  }

  function autofocus(node: HTMLInputElement) {
    node.focus();
    node.select();
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
            <SettingRow
              title="Send only the safe parts to the cloud"
              desc="Strip private details before asking a cloud model"
            >
              {#snippet control()}
                <Toggle checked={redaction} onchange={(v) => (redaction = v)} />
              {/snippet}
            </SettingRow>
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
          {:else if section === "models"}
            <SettingRow
              title="Qwen3-14B"
              desc="Local · tadashi"
              dotColor="var(--local)"
              tag={{ label: "online", bg: "var(--local-soft)", color: "var(--local)" }}
            />
            <SettingRow
              title="Llama 3.3 8B"
              desc="Local · on this Mac"
              dotColor="var(--local)"
              tag={{ label: "ready", bg: "var(--surface-2)", color: "var(--text-3)" }}
            />
            <SettingRow
              title="Anthropic"
              desc="Cloud · Opus 4.8, Sonnet 5"
              dotColor="var(--cloud)"
              tag={{ label: "connected", bg: "var(--cloud-soft)", color: "var(--cloud)" }}
            >
              {#snippet control()}
                <Button variant="ghost">Manage…</Button>
              {/snippet}
            </SettingRow>
            <button
              type="button"
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
          {:else if section === "memory"}
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
                  onchange={(v) => (memoryMode = v)}
                />
              {/snippet}
            </SettingRow>
            <div class="mb-2 mt-4 flex items-center gap-2.5">
              <div class="relative flex-shrink-0">
                <button
                  type="button"
                  onclick={() => (memMenuOpen = !memMenuOpen)}
                  aria-haspopup="true"
                  class="flex min-w-[150px] cursor-pointer items-center gap-2 rounded-[var(--r)] border border-border-strong
                    bg-surface-2 px-[9px] py-1.5 text-[12.5px] font-[550] text-text"
                >
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" class="flex-shrink-0 text-text-3">
                    <circle cx="12" cy="8" r="4" />
                    <path d="M4 21c1.5-4 5-6 8-6s6.5 2 8 6" />
                  </svg>
                  <span class="flex-1 text-left">{memProfile}</span>
                  <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="flex-shrink-0 text-text-3">
                    <path d="M6 9l6 6 6-6" />
                  </svg>
                </button>
                {#if memMenuOpen}
                  <div
                    class="absolute left-0 top-9 z-[5] max-h-[230px] w-[230px] overflow-y-auto rounded-[var(--r-lg)]
                      border border-border-strong bg-surface p-[5px] shadow-[var(--shadow-pop)]"
                  >
                    {#each Object.keys(memories) as name (name)}
                      <button
                        type="button"
                        onclick={() => {
                          memProfile = name;
                          memMenuOpen = false;
                          editingMem = null;
                        }}
                        class="flex w-full cursor-pointer items-center gap-2 rounded-[var(--r)] border-0 bg-transparent px-[9px] py-[7px] text-left text-text"
                      >
                        <span class="min-w-0 flex-1 text-[12.5px] font-[550]">{name}</span>
                        <span class="flex-shrink-0 text-[11px] text-text-3">{memories[name].length}</span>
                        {#if name === memProfile}
                          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="var(--accent)" stroke-width="2.4" class="flex-shrink-0">
                            <path d="M4 12l5 5L20 7" />
                          </svg>
                        {/if}
                      </button>
                    {/each}
                    <div class="mx-1 my-[5px] h-px bg-border"></div>
                    <button
                      type="button"
                      class="flex w-full cursor-pointer items-center gap-2 rounded-[var(--r)] border-0 bg-transparent px-[9px] py-2 text-left text-[12.5px] font-medium text-text-2"
                    >
                      <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                        <path d="M12 5v14M5 12h14" />
                      </svg>
                      New profile…
                    </button>
                  </div>
                {/if}
              </div>
              <span class="text-[11.5px] text-text-3">{mems.length} memories · stored locally, per profile</span>
              <div class="flex-1"></div>
              <Button variant="ghost">Export</Button>
              <Button variant="ghost">Forget all…</Button>
            </div>

            <div class="flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border">
              {#each mems as m, i (i)}
                <div class="flex items-center gap-2 border-b border-border py-[7px] pl-3 pr-2.5">
                  {#if editingMem === i}
                    <input
                      use:autofocus
                      value={m.text}
                      aria-label="Edit memory"
                      onkeydown={(e) => {
                        if (e.key === "Enter") {
                          e.preventDefault();
                          e.currentTarget.blur();
                        } else if (e.key === "Escape") {
                          e.preventDefault();
                          cancelEdit = true;
                          e.currentTarget.blur();
                        }
                      }}
                      onblur={(e) => commitMem(i, e.currentTarget.value)}
                      class="min-w-0 flex-1 rounded-[var(--r)] border border-accent bg-surface-2 px-2 py-[5px] text-[12.5px] text-text outline-none"
                    />
                  {:else}
                    <button
                      type="button"
                      onclick={() => (editingMem = i)}
                      title="Click to edit"
                      class="min-w-0 flex-1 cursor-text rounded-[4px] border-0 bg-transparent px-0.5 py-1 text-left text-[12.5px] text-text"
                    >
                      {m.text}
                    </button>
                  {/if}
                  {#if m.tag}
                    <span
                      class="flex-shrink-0 rounded-[8px] px-[7px] py-px text-[10px]
                        {m.tag.includes('health')
                        ? 'bg-blocked-soft text-blocked'
                        : 'bg-warn-soft text-warn'}"
                    >
                      {m.tag}
                    </span>
                  {/if}
                  <span class="flex-shrink-0 text-[11px] text-text-3">{m.date}</span>
                  <button
                    type="button"
                    aria-label="Edit this memory"
                    onclick={() => (editingMem = i)}
                    class="grid h-6 w-6 place-items-center rounded-[var(--r)] border-0 bg-transparent text-text-3 hover:bg-surface-hover hover:text-text"
                  >
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
                      <path d="M5 19l1-4L16 5l3 3L9 18z" />
                    </svg>
                  </button>
                  <button
                    type="button"
                    aria-label="Forget this memory"
                    onclick={() => updateMems((arr) => arr.filter((_, j) => j !== i))}
                    class="grid h-6 w-6 place-items-center rounded-[var(--r)] border-0 bg-transparent text-text-3 hover:bg-surface-hover hover:text-text"
                  >
                    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                      <path d="M6 6l12 12M18 6 6 18" />
                    </svg>
                  </button>
                </div>
              {/each}
              <button
                type="button"
                onclick={addMemory}
                class="flex w-full cursor-pointer items-center gap-2 border-0 border-b border-border bg-transparent px-3 py-[9px] text-left text-[12.5px] text-text-2"
              >
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="flex-shrink-0">
                  <path d="M12 5v14M5 12h14" />
                </svg>
                Add a memory
              </button>
              <div class="px-3 py-2 text-[11px] text-text-3">
                Click any memory to edit it. Memories are written only with your
                approval and stay on this Mac.
              </div>
            </div>
          {:else if section === "appearance"}
            <SettingRow title="Theme" desc="Follow the system, or pick one">
              {#snippet control()}
                <SegmentedControl
                  options={[
                    { value: "dark", label: "Dark" },
                    { value: "light", label: "Light" },
                    { value: "system", label: "System" },
                  ]}
                  value={themeSel}
                  onchange={applyTheme}
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
