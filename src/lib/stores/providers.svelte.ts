// Lost Harness — Provider store (Svelte 5 runes).
//
// Tracks the list of model providers (base URL, API key, kind) and the
// user's currently-active provider/model selection.
//
// M1: round-trips through the real IPC layer (add_provider, remove_provider,
// list_providers, list_models). localStorage is kept as the browser-only
// fallback for development without the Tauri shell.
//
// Shape matches the catalog in the Electron prototype
// (lost-harness-app/src/store/catalog.js) so swapping the backing store
// later is a no-op for the UI.

import * as api from "../api/tauri";

export type ProviderKind = "local" | "cloud" | "custom";

export interface Provider {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  kind: ProviderKind;
  /** Whether the backend classified the endpoint as private/LAN. */
  isPrivate: boolean;
  /** Q1: whether the endpoint supports OpenAI-style native structured tool calls. */
  supportsNativeTools: boolean;
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
  /** True while hydrateProviders() is in flight. */
  loading: false as boolean,
});

// Cache of fetched models per provider id — avoids re-fetching on every
// picker open. The catalog store still provides baked-in presets as a
// fallback.
const modelCache = new Map<string, string[]>();

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

// ── Hydration from backend ──────────────────────────────────────────────────

/**
 * Loads providers from the backend IPC (or browser fallback). Call once on
 * app start. Merges with any locally-stored providers and persists the
 * combined list back to localStorage as the fallback cache.
 */
export async function hydrateProviders(): Promise<void> {
  providersStore.loading = true;
  try {
    const remote = await api.listProviders();
    // Map ProviderInfo → Provider (camelCase for the frontend).
    const mapped: Provider[] = remote.map((p) => ({
      id: p.id,
      name: p.name,
      baseUrl: p.base_url,
      apiKey: "", // backend omits the key; user must re-enter to edit
      kind: (p.kind as ProviderKind) ?? "custom",
      isPrivate: p.is_private,
      supportsNativeTools: p.supports_native_tools,
    }));

    // Merge: if a local provider has the same id, keep the local apiKey
    // (the backend never returns it). Otherwise add the remote one.
    const localById = new Map(providersStore.providers.map((p) => [p.id, p]));
    const merged: Provider[] = mapped.map((p) => {
      const local = localById.get(p.id);
      return local ? { ...p, apiKey: local.apiKey } : p;
    });
    // Add any local providers not known to the backend (browser fallback
    // might have created them).
    for (const [id, p] of localById) {
      if (!merged.some((m) => m.id === id)) merged.push(p);
    }

    providersStore.providers = merged;
    // Ensure active selection still valid.
    if (
      providersStore.activeProviderId &&
      !merged.some((p) => p.id === providersStore.activeProviderId)
    ) {
      providersStore.activeProviderId = merged[0]?.id ?? null;
      providersStore.activeModel = null;
    }
    if (!providersStore.activeProviderId && merged.length > 0) {
      providersStore.activeProviderId = merged[0].id;
    }
    persistProviders();
    persistActive();
  } catch (err) {
    console.error("hydrateProviders failed", err);
  } finally {
    providersStore.loading = false;
  }
}

/**
 * Fetches the model list for a provider via the backend IPC and caches the
 * result. Returns the cached list if available. In browser fallback, returns
 * a minimal list.
 */
export async function fetchModels(providerId: string): Promise<string[]> {
  const cached = modelCache.get(providerId);
  if (cached) return cached;
  try {
    const models = await api.listModels(providerId);
    modelCache.set(providerId, models);
    return models;
  } catch (err) {
    console.error("fetchModels failed", err);
    return [];
  }
}

/** Add a new provider via the backend IPC. Falls back to localStorage in browser mode. */
export async function addProvider(
  p: Omit<Provider, "id" | "isPrivate"> & { id?: string },
): Promise<Provider> {
  const existingIdx = providersStore.providers.findIndex((x) => x.id === p.id);

  if (existingIdx >= 0) {
    // Update existing — round-trip through the backend so the edit
    // survives a restart (hydrateProviders re-pulls from global.db).
    const id = p.id!;
    const existing = providersStore.providers[existingIdx];
    // The edit form leaves the key field blank rather than echoing the
    // stored secret; blank means "keep the stored key".
    const apiKey = p.apiKey || existing.apiKey;
    try {
      const info = await api.updateProvider(
        id,
        p.name.trim(),
        p.baseUrl.trim(),
        p.apiKey || null,
        p.kind,
        p.supportsNativeTools,
      );
      providersStore.providers[existingIdx] = {
        id: info.id,
        name: info.name,
        baseUrl: info.base_url,
        apiKey,
        kind: (info.kind as ProviderKind) ?? p.kind,
        isPrivate: info.is_private,
        supportsNativeTools: info.supports_native_tools,
      };
    } catch (err) {
      // IPC error — update locally so the UI reflects the edit for this
      // session; it may revert on next launch if the backend never saw it.
      console.error("updateProvider IPC failed, using local fallback", err);
      providersStore.providers[existingIdx] = {
        id,
        name: p.name.trim(),
        baseUrl: p.baseUrl.trim(),
        apiKey,
        kind: p.kind,
        isPrivate: existing.isPrivate,
        supportsNativeTools: p.supportsNativeTools,
      };
    }
    // Base URL or kind may have changed — drop the cached model list.
    modelCache.delete(id);
    persistProviders();
    return providersStore.providers[existingIdx];
  }

  // New provider — round-trip through the backend.
  try {
    const info = await api.addProvider(
      p.name.trim(),
      p.baseUrl.trim(),
      p.apiKey || null,
      p.kind,
      p.supportsNativeTools,
    );
    const provider: Provider = {
      id: info.id,
      name: info.name,
      baseUrl: info.base_url,
      apiKey: p.apiKey ?? "",
      kind: (info.kind as ProviderKind) ?? p.kind,
      isPrivate: info.is_private,
      supportsNativeTools: info.supports_native_tools,
    };
    providersStore.providers.push(provider);
    // First provider added → make it active by default.
    if (providersStore.activeProviderId === null) {
      providersStore.activeProviderId = provider.id;
    }
    persistProviders();
    persistActive();
    return provider;
  } catch (err) {
    // Browser fallback or IPC error — create locally with a generated id.
    console.error("addProvider IPC failed, using local fallback", err);
    const id = p.id ?? newId();
    const next: Provider = {
      id,
      name: p.name.trim(),
      baseUrl: p.baseUrl.trim(),
      apiKey: p.apiKey ?? "",
      kind: p.kind,
      isPrivate: p.kind === "local",
      supportsNativeTools: p.supportsNativeTools,
    };
    providersStore.providers.push(next);
    if (providersStore.activeProviderId === null) {
      providersStore.activeProviderId = id;
    }
    persistProviders();
    persistActive();
    return next;
  }
}

/** Remove a provider by id via the backend IPC. Falls back to localStorage. */
export async function removeProvider(id: string): Promise<void> {
  try {
    await api.removeProvider(id);
  } catch (err) {
    console.error("removeProvider IPC failed, using local fallback", err);
  }
  // Always update local state regardless of IPC result.
  const idx = providersStore.providers.findIndex((p) => p.id === id);
  if (idx < 0) return;
  providersStore.providers.splice(idx, 1);
  modelCache.delete(id);
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
  modelCache.clear();
  persistProviders();
  persistActive();
}

// Quick lookup for the UI; pure derived data off the runes state.
export function getProvider(id: string | null): Provider | null {
  if (!id) return null;
  return providersStore.providers.find((p) => p.id === id) ?? null;
}
