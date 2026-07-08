// Lost Harness — Provider store (Svelte 5 runes).
//
// Tracks the list of model providers (base URL, API key, kind) and the
// user's currently-active provider/model selection. M1 stub: persists to
// localStorage; the real impl will round-trip through the Rust config
// store (sled/redb) via Tauri IPC.
//
// Shape matches the catalog in the Electron prototype
// (lost-harness-app/src/store/catalog.js) so swapping the backing store
// later is a no-op for the UI.

export type ProviderKind = "local" | "cloud" | "custom";

export interface Provider {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  kind: ProviderKind;
}

const STORAGE_KEY = "lh.providers.v1";
const ACTIVE_KEY = "lh.providers.active.v1";

function newId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return "p-" + Math.random().toString(36).slice(2) + Date.now().toString(36);
}

function loadFromStorage(): {
  providers: Provider[];
  activeProviderId: string | null;
  activeModel: string | null;
} {
  const empty = { providers: [], activeProviderId: null, activeModel: null };
  if (typeof localStorage === "undefined") return empty;
  try {
    const rawProv = localStorage.getItem(STORAGE_KEY);
    const rawActive = localStorage.getItem(ACTIVE_KEY);
    const providers: Provider[] = rawProv ? JSON.parse(rawProv) : [];
    let activeProviderId: string | null = null;
    let activeModel: string | null = null;
    if (rawActive) {
      const parsed = JSON.parse(rawActive) as {
        providerId?: string | null;
        model?: string | null;
      };
      activeProviderId = parsed.providerId ?? null;
      activeModel = parsed.model ?? null;
    }
    return { providers, activeProviderId, activeModel };
  } catch {
    return empty;
  }
}

const initial = loadFromStorage();

// Runes. `$state` here means a single shared store object that all
// subscribers (the components) read directly. In a `.svelte.ts` module the
// state lives on the module record, so a single import is enough.
export const providersStore = $state({
  providers: initial.providers as Provider[],
  activeProviderId: initial.activeProviderId as string | null,
  activeModel: initial.activeModel as string | null,
});

function persistProviders(): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(providersStore.providers));
  } catch {
    // localStorage may be unavailable; non-fatal.
  }
}

function persistActive(): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(
      ACTIVE_KEY,
      JSON.stringify({
        providerId: providersStore.activeProviderId,
        model: providersStore.activeModel,
      }),
    );
  } catch {
    // non-fatal
  }
}

/** Add a new provider or replace an existing one (matched by `p.id`). */
export function addProvider(p: Omit<Provider, "id"> & { id?: string }): Provider {
  const id = p.id ?? newId();
  const existingIdx = providersStore.providers.findIndex((x) => x.id === id);
  const next: Provider = {
    id,
    name: p.name.trim(),
    baseUrl: p.baseUrl.trim(),
    apiKey: p.apiKey ?? "",
    kind: p.kind,
  };
  if (existingIdx >= 0) {
    providersStore.providers[existingIdx] = next;
  } else {
    providersStore.providers.push(next);
  }
  // First provider added → make it active by default.
  if (providersStore.activeProviderId === null) {
    providersStore.activeProviderId = id;
  }
  persistProviders();
  return next;
}

/** Remove a provider by id. Clears the active selection if it pointed here. */
export function removeProvider(id: string): void {
  const idx = providersStore.providers.findIndex((p) => p.id === id);
  if (idx < 0) return;
  providersStore.providers.splice(idx, 1);
  if (providersStore.activeProviderId === id) {
    providersStore.activeProviderId = providersStore.providers[0]?.id ?? null;
    providersStore.activeModel = null;
    persistActive();
  }
  persistProviders();
}

/** Switch the active selection. No-op if the provider doesn't exist. */
export function setActiveModel(providerId: string, model: string): void {
  const exists = providersStore.providers.some((p) => p.id === providerId);
  if (!exists) return;
  providersStore.activeProviderId = providerId;
  providersStore.activeModel = model;
  persistActive();
}

/** Reset everything — used in tests / future "clear all" UI. */
export function resetProviders(): void {
  providersStore.providers = [];
  providersStore.activeProviderId = null;
  providersStore.activeModel = null;
  persistProviders();
  persistActive();
}

// Quick lookup for the UI; pure derived data off the runes state.
export function getProvider(id: string | null): Provider | null {
  if (!id) return null;
  return providersStore.providers.find((p) => p.id === id) ?? null;
}
