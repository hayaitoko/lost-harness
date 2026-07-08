<script lang="ts">
  // Lost Harness — Provider settings (modal/overlay form).
  //
  // Reference: lost-harness-app/src/components/ProvidersSection.jsx
  // M1 stub version: no bridge round-trip, no secure-storage check, no
  // live /models test. Just list + add/edit/delete + quick-add presets
  // writing to the in-memory + localStorage store.
  //
  // Quick-add buttons pre-fill the form (id + base URL + kind) so the
  // user only has to type their name (auto-filled) and paste an API key.

  import {
    providersStore,
    addProvider,
    removeProvider,
    type Provider,
    type ProviderKind,
  } from "$lib/stores/providers.svelte";

  interface Props {
    /** When true, render as a full-screen modal overlay with a backdrop. */
    modal?: boolean;
    /** Close handler (only used when `modal=true`). */
    onclose?: () => void;
  }

  let { modal = true, onclose }: Props = $props();

  // Quick-add presets. Mirrors the matching entries in the Electron
  // prototype's PROVIDER_CATALOG.
  const QUICK_PRESETS: Array<{
    id: string;
    name: string;
    baseUrl: string;
    kind: ProviderKind;
  }> = [
    { id: "openai", name: "OpenAI", baseUrl: "https://api.openai.com/v1", kind: "cloud" },
    { id: "anthropic", name: "Anthropic", baseUrl: "https://api.anthropic.com/v1", kind: "cloud" },
    {
      id: "openrouter",
      name: "OpenRouter",
      baseUrl: "https://openrouter.ai/api/v1",
      kind: "cloud",
    },
    {
      id: "lmstudio",
      name: "LM Studio",
      baseUrl: "http://localhost:1234/v1",
      kind: "local",
    },
    { id: "ollama", name: "Ollama", baseUrl: "http://127.0.0.1:11434/v1", kind: "local" },
  ];

  interface FormState {
    id: string | null;
    name: string;
    baseUrl: string;
    apiKey: string;
    kind: ProviderKind;
  }

  const EMPTY_FORM: FormState = {
    id: null,
    name: "",
    baseUrl: "",
    apiKey: "",
    kind: "cloud",
  };

  let form: FormState | null = $state(null);
  let showKey = $state(false);
  let confirmRemoveId: string | null = $state(null);
  let saving = $state(false);

  // Providers are hydrated once at app start (App.svelte). Re-hydrating on
  // every modal open would clobber unsaved local edits (there is no
  // update_provider IPC yet), so this component just reads the store.

  // Derived: name + URL required for save to be enabled.
  const canSave = $derived.by(() => {
    if (!form) return false;
    const name = form.name.trim();
    const url = form.baseUrl.trim();
    if (!name || !url) return false;
    if (!/^https?:\/\//.test(url)) return false;
    return true;
  });

  function startAdd() {
    form = { ...EMPTY_FORM };
    showKey = false;
  }

  function startAddFromPreset(preset: (typeof QUICK_PRESETS)[number]) {
    form = {
      id: null,
      name: preset.name,
      baseUrl: preset.baseUrl,
      apiKey: "",
      kind: preset.kind,
    };
    showKey = false;
  }

  function startEdit(p: Provider) {
    form = {
      id: p.id,
      name: p.name,
      baseUrl: p.baseUrl,
      apiKey: "",
      kind: p.kind,
    };
    showKey = false;
  }

  function cancelForm() {
    form = null;
  }

  async function save() {
    if (!form || !canSave || saving) return;
    saving = true;
    try {
      await addProvider({
        id: form.id ?? undefined,
        name: form.name.trim(),
        baseUrl: form.baseUrl.trim(),
        apiKey: form.apiKey,
        kind: form.kind,
      });
      form = null;
    } catch (err) {
      console.error("save provider failed", err);
    } finally {
      saving = false;
    }
  }

  async function handleRemoveClick(id: string) {
    // Two-click confirm: first click arms it, second click within 3s fires.
    if (confirmRemoveId !== id) {
      confirmRemoveId = id;
      setTimeout(() => {
        if (confirmRemoveId === id) confirmRemoveId = null;
      }, 3000);
      return;
    }
    confirmRemoveId = null;
    await removeProvider(id);
  }

  function close() {
    onclose?.();
  }

  function onBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) close();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape" && modal) {
      e.preventDefault();
      close();
    }
  }

  // Cycle the binding dot color for visual parity with the picker.
  function kindClass(kind: ProviderKind): string {
    if (kind === "local") return "bg-amber-500/10 text-amber-400";
    if (kind === "custom") return "bg-neutral-500/10 text-neutral-400";
    return "bg-emerald-500/10 text-emerald-400";
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if modal}
  <div
    class="prov-modal-backdrop fixed inset-0 z-40 flex items-center justify-center bg-black/60 p-4"
    onclick={onBackdropClick}
    role="presentation"
    data-testid="provider-settings-backdrop"
  >
    <div
      class="prov-modal relative flex max-h-[90vh] w-full max-w-2xl flex-col overflow-hidden rounded-2xl border border-neutral-800 bg-neutral-950 shadow-2xl"
      role="dialog"
      aria-modal="true"
      aria-label="Provider settings"
      data-testid="provider-settings-modal"
    >
      {@render body()}
    </div>
  </div>
{:else}
  {@render body()}
{/if}

{#snippet body()}
  <!-- Header -->
  <header
    class="flex shrink-0 items-center justify-between border-b border-neutral-800 px-5 py-3"
  >
    <div>
      <h2 class="text-base font-semibold text-neutral-100">Providers</h2>
      <p class="text-xs text-neutral-500">
        OpenAI-compatible endpoints your agent can route through.
      </p>
    </div>
    {#if modal}
      <button
        type="button"
        class="rounded-md p-1 text-neutral-500 transition hover:bg-neutral-900 hover:text-neutral-200"
        onclick={close}
        aria-label="Close"
      >
        <svg class="h-4 w-4" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M3 3 L13 13 M13 3 L3 13"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
          />
        </svg>
      </button>
    {/if}
  </header>

  <!-- Body: scrollable list + form -->
  <div class="flex-1 overflow-y-auto px-5 py-4">
    <!-- Configured providers -->
    {#if providersStore.providers.length > 0}
      <ul class="space-y-2" data-testid="provider-list">
        {#each providersStore.providers as p (p.id)}
          {@const isEditing = form?.id === p.id}
          {@const isConfirming = confirmRemoveId === p.id}
          <li
            class="flex items-center justify-between gap-3 rounded-lg border border-neutral-800 bg-neutral-900/60 px-3 py-2"
          >
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-2">
                <span
                  class="inline-block h-1.5 w-1.5 rounded-full {p.kind === 'local'
                    ? 'bg-amber-500'
                    : p.kind === 'cloud'
                      ? 'bg-emerald-500'
                      : 'bg-neutral-500'}"
                  aria-hidden="true"
                ></span>
                <span class="truncate text-sm font-medium text-neutral-100">
                  {p.name}
                </span>
                <span
                  class="rounded px-1.5 py-0.5 text-[10px] font-medium {kindClass(
                    p.kind,
                  )}"
                >
                  {p.kind}
                </span>
              </div>
              <p class="mt-0.5 truncate text-[11px] text-neutral-500">{p.baseUrl}</p>
            </div>
            <div class="flex shrink-0 items-center gap-1">
              <button
                type="button"
                class="rounded-md px-2 py-1 text-xs text-neutral-300 transition hover:bg-neutral-800"
                onclick={() => (isEditing ? cancelForm() : startEdit(p))}
                data-testid="provider-edit"
              >
                {isEditing ? "Cancel" : "Edit"}
              </button>
              <button
                type="button"
                class="rounded-md px-2 py-1 text-xs text-rose-400 transition hover:bg-rose-500/10"
                onclick={() => handleRemoveClick(p.id)}
                data-testid="provider-remove"
              >
                {isConfirming ? "Confirm?" : "Remove"}
              </button>
            </div>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="rounded-lg border border-dashed border-neutral-800 px-3 py-6 text-center text-sm text-neutral-500">
        No providers configured yet. Add one below.
      </p>
    {/if}

    <!-- Quick-add presets -->
    <div class="mt-5">
      <p class="mb-2 text-[10px] font-semibold uppercase tracking-wider text-neutral-500">
        Quick add
      </p>
      <div class="flex flex-wrap gap-2" data-testid="provider-quick-add">
        {#each QUICK_PRESETS as preset (preset.id)}
          <button
            type="button"
            class="rounded-md border border-neutral-800 bg-neutral-900 px-2.5 py-1 text-xs text-neutral-200 transition hover:border-neutral-700 hover:bg-neutral-800"
            onclick={() => startAddFromPreset(preset)}
            data-testid="provider-preset"
            data-preset-id={preset.id}
          >
            {preset.name}
          </button>
        {/each}
      </div>
    </div>

    <!-- Add / edit form -->
    {#if form}
      <form
        class="mt-5 space-y-3 rounded-lg border border-neutral-800 bg-neutral-900/60 p-4"
        onsubmit={(e) => {
          e.preventDefault();
          save();
        }}
        data-testid="provider-form"
      >
        <div>
          <label class="mb-1 block text-[10px] font-semibold uppercase tracking-wider text-neutral-500" for="prov-name">
            Name
          </label>
          <input
            id="prov-name"
            type="text"
            class="w-full rounded-md border border-neutral-800 bg-neutral-950 px-2.5 py-1.5 text-sm text-neutral-100 placeholder:text-neutral-600 focus:border-indigo-500 focus:outline-none"
            placeholder="My OpenAI"
            value={form.name}
            oninput={(e) =>
              (form = { ...form!, name: (e.currentTarget as HTMLInputElement).value })}
          />
        </div>

        <div>
          <label class="mb-1 block text-[10px] font-semibold uppercase tracking-wider text-neutral-500" for="prov-url">
            Base URL
          </label>
          <input
            id="prov-url"
            type="text"
            class="w-full rounded-md border border-neutral-800 bg-neutral-950 px-2.5 py-1.5 text-sm text-neutral-100 placeholder:text-neutral-600 focus:border-indigo-500 focus:outline-none"
            placeholder="https://api.openai.com/v1"
            value={form.baseUrl}
            oninput={(e) =>
              (form = {
                ...form!,
                baseUrl: (e.currentTarget as HTMLInputElement).value,
              })}
          />
        </div>

        <div>
          <label class="mb-1 block text-[10px] font-semibold uppercase tracking-wider text-neutral-500" for="prov-key">
            API key
          </label>
          <div class="flex gap-2">
            <input
              id="prov-key"
              type={showKey ? "text" : "password"}
              class="flex-1 rounded-md border border-neutral-800 bg-neutral-950 px-2.5 py-1.5 text-sm text-neutral-100 placeholder:text-neutral-600 focus:border-indigo-500 focus:outline-none"
              placeholder={form.id ? "••• saved — leave blank to keep" : "sk-…"}
              value={form.apiKey}
              oninput={(e) =>
                (form = {
                  ...form!,
                  apiKey: (e.currentTarget as HTMLInputElement).value,
                })}
            />
            <button
              type="button"
              class="rounded-md border border-neutral-800 bg-neutral-950 px-2 text-xs text-neutral-400 transition hover:bg-neutral-800"
              onclick={() => (showKey = !showKey)}
              aria-label={showKey ? "Hide API key" : "Show API key"}
            >
              {showKey ? "Hide" : "Show"}
            </button>
          </div>
        </div>

        <div>
          <label class="mb-1 block text-[10px] font-semibold uppercase tracking-wider text-neutral-500" for="prov-kind">
            Kind
          </label>
          <select
            id="prov-kind"
            class="w-full rounded-md border border-neutral-800 bg-neutral-950 px-2.5 py-1.5 text-sm text-neutral-100 focus:border-indigo-500 focus:outline-none"
            value={form.kind}
            onchange={(e) =>
              (form = {
                ...form!,
                kind: (e.currentTarget as HTMLSelectElement).value as ProviderKind,
              })}
          >
            <option value="cloud">cloud (public endpoint)</option>
            <option value="local">local (loopback / LAN / tailnet)</option>
            <option value="custom">custom (other)</option>
          </select>
        </div>

        <div class="flex items-center justify-end gap-2 pt-1">
          <button
            type="button"
            class="rounded-md px-3 py-1.5 text-sm text-neutral-300 transition hover:bg-neutral-800"
            onclick={cancelForm}
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={!canSave || saving}
            class="rounded-md bg-indigo-600 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
            data-testid="provider-save"
          >
            {saving ? "Saving…" : form.id ? "Save changes" : "Add provider"}
          </button>
        </div>
      </form>
    {:else if providersStore.providers.length === 0}
      <div class="mt-5 flex justify-center">
        <button
          type="button"
          class="rounded-md border border-neutral-800 bg-neutral-900 px-3 py-1.5 text-sm font-medium text-neutral-200 transition hover:border-neutral-700 hover:bg-neutral-800"
          onclick={startAdd}
          data-testid="provider-add-custom"
        >
          + Add custom provider
        </button>
      </div>
    {/if}
  </div>
{/snippet}
