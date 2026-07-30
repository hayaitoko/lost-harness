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
import { invoke } from "@tauri-apps/api/core";

export type ProviderKind = "local" | "cloud" | "custom";

export interface Provider {
  id: string;
  name: string;
  baseUrl: string;
  /** Whether a key has been stored for this provider. The key itself lives
   *  in the OS keychain and NEVER enters the renderer process. */
  hasApiKey: boolean;
  kind: ProviderKind;
  /** Whether the backend classified the endpoint as private/LAN. */
  isPrivate: boolean;
  /** Q1: whether the endpoint supports OpenAI-style native structured tool calls. */
  supportsNativeTools: boolean;
  /** F5: the endpoint is trusted as "private" by its DNS/mDNS NAME
   * (.local/.lan/.internal/.ts.net) rather than a loopback/RFC1918 IP
   * literal — surfaced so the UI can show the "only on a network you
   * control" notice. */
  trustedByName: boolean;
}

/** Input shape for addProvider — the caller (Settings UI) provides an
 *  apiKey string, but the store never keeps it in the frontend model.
 *  It is sent directly to the OS keychain via the one-shot IPC and
 *  immediately discarded from renderer memory. */
export interface ProviderInput {
  id?: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  kind: ProviderKind;
  supportsNativeTools: boolean;
}

const STORAGE_KEY = "lh.providers.v1";
const ACTIVE_KEY = "lh.providers.active.v1";

/** Migrate old stored data: strip `apiKey` from any stored Provider objects
 *  (replaced with a `hasApiKey` boolean). Also strips apiKey from any
 *  residual objects that slipped through before this fix. */
function migrateProviders(raw: unknown): Provider[] {
  if (!Array.isArray(raw)) return [];
  return raw.map((p: Record<string, unknown>) => {
    const hadKey =
      typeof p.apiKey === "string" && (p.apiKey as string).length > 0;
    const { apiKey: _, ...rest } = p;
    return { ...rest, hasApiKey: hadKey } as unknown as Provider;
  });
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
    const providers: Provider[] = rawProv
      ? migrateProviders(JSON.parse(rawProv))
      : [];
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
// picker open. The installed app never substitutes a baked-in model list.
const modelCache = new Map<string, string[]>();

function persistProviders(): void {
  if (typeof localStorage === "undefined") return;
  try {
    // Strip any residual apiKey field before persisting (defense in depth).
    const cleaned = providersStore.providers.map((p) => ({
      id: p.id,
      name: p.name,
      baseUrl: p.baseUrl,
      hasApiKey: p.hasApiKey,
      kind: p.kind,
      isPrivate: p.isPrivate,
      supportsNativeTools: p.supportsNativeTools,
      trustedByName: p.trustedByName,
    }));
    localStorage.setItem(STORAGE_KEY, JSON.stringify(cleaned));
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

// On startup, immediately overwrite any legacy localStorage payload with the
// already-stripped in-memory providers (migrateProviders() dropped `apiKey`
// during load). This ensures a plaintext key from a pre-fix build cannot
// linger on disk until the first hydrate/persist cycle. If the stored blob
// was unparseable, providers is [] and we simply clear it — fail closed.
if (
  typeof localStorage !== "undefined" &&
  localStorage.getItem(STORAGE_KEY) !== null
) {
  persistProviders();
}

// ── One-shot key injection IPC ────────────────────────────────────────────

/**
 * Sends an API key to the backend via the dedicated `set_provider_api_key`
 * one-shot IPC. The backend stores it in the OS keychain. The frontend
 * NEVER retains the key in memory or localStorage.
 *
 * SECURITY: this function does NOT swallow IPC failures. If the durable
 * write fails, the rejection propagates so callers keep the provider in a
 * truthful state (unconfigured / un-rotated) instead of reporting a phantom
 * success.
 *
 * Returns `true` only when the key was handed to a real keychain-backed
 * backend. In browser fallback mode (no Tauri shell, therefore no keychain)
 * the plaintext key is dropped and `false` is returned, so callers must NOT
 * flip `hasApiKey` — claiming a stored key there would be the same lie the
 * IPC-failure path used to tell.
 */
async function setProviderApiKey(
  providerId: string,
  apiKey: string,
): Promise<boolean> {
  // Browser fallback: no keychain available — the plaintext key is dropped
  // and never persisted. Nothing was durably stored, so report that.
  if (!api.isTauriRuntime()) return false;

  // Tauri runtime: hand the key to the backend one-shot IPC. Deliberately
  // NOT wrapped in try/catch — a rejection must reach the caller.
  await invoke("set_provider_api_key", {
    args: { provider_id: providerId, api_key: apiKey },
  });
  return true;
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
    // The backend never returns the API key itself; it may or may not
    // return has_api_key (P05 adds this). We default to false and rely
    // on the local-merge below until the backend catches up.
    const mapped: Provider[] = remote.map((p) => ({
      id: p.id,
      name: p.name,
      baseUrl: p.base_url,
      hasApiKey: (p as any).has_api_key ?? false,
      kind: (p.kind as ProviderKind) ?? "custom",
      isPrivate: p.is_private,
      supportsNativeTools: p.supports_native_tools,
      trustedByName: p.trusted_by_name,
    }));

    // Merge: if a local provider has the same id, preserve the local
    // hasApiKey (the backend doesn't return it yet; P05 adds it).
    const localById = new Map(providersStore.providers.map((p) => [p.id, p]));
    const merged: Provider[] = mapped.map((p) => {
      const local = localById.get(p.id);
      return local ? { ...p, hasApiKey: local.hasApiKey } : p;
    });
    // Browser fallback owns its local store. In the installed app, treating
    // an unknown cache entry as real would resurrect a failed/deleted
    // provider after restart and falsely imply it was saved.
    if (!api.isTauriRuntime()) {
      for (const [id, p] of localById) {
        if (!merged.some((m) => m.id === id)) merged.push(p);
      }
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

/** Add a provider through the backend (or the bridge's browser-mode mock).
 *  The caller provides an apiKey string; the store sends it to the backend
 *  via the secure one-shot IPC (`set_provider_api_key`) and immediately
 *  drops it — only `hasApiKey: boolean` is kept in the frontend model. */
export async function addProvider(input: ProviderInput): Promise<Provider> {
  const existingIdx = providersStore.providers.findIndex(
    (x) => x.id === input.id,
  );

  if (existingIdx >= 0) {
    // Update existing — round-trip through the backend so the edit
    // survives a restart (hydrateProviders re-pulls from global.db).
    const id = input.id!;
    const existing = providersStore.providers[existingIdx];
    // If the form had a non-blank key, send it via one-shot IPC; blank
    // means "keep the existing key" (already stored in the OS keychain).
    const gotNewKey = !!input.apiKey;

    let info;
    try {
      info = await api.updateProvider(
        id,
        input.name.trim(),
        input.baseUrl.trim(),
        null, // key never flows through the provider-update IPC
        input.kind,
        input.supportsNativeTools,
      );
    } catch (err) {
      // The bridge already supplies a browser-only mock. A real Tauri
      // failure must stay visible rather than pretending a durable edit won.
      console.error("updateProvider failed", err);
      throw err;
    }

    // Metadata is durably updated. Reflect it now, but leave hasApiKey at its
    // previous value — it flips only after (and unless) a new key durably
    // rotates in below.
    providersStore.providers[existingIdx] = {
      id: info.id,
      name: info.name,
      baseUrl: info.base_url,
      hasApiKey: existing.hasApiKey,
      kind: (info.kind as ProviderKind) ?? input.kind,
      isPrivate: info.is_private,
      supportsNativeTools: info.supports_native_tools,
      trustedByName: info.trusted_by_name,
    };
    // Base URL or kind may have changed — drop the cached model list.
    modelCache.delete(id);
    persistProviders();

    if (gotNewKey) {
      // Rotate the key. If this rejects, the provider keeps its previous
      // hasApiKey state (the old key is still valid) and the error surfaces
      // to the UI — never a silent "rotation succeeded". Clear the plaintext
      // input as soon as it has been handed off.
      const stored = await setProviderApiKey(id, input.apiKey);
      input.apiKey = "";
      // Only claim a stored key when a keychain-backed backend took it.
      if (stored) {
        providersStore.providers[existingIdx].hasApiKey = true;
        persistProviders();
      }
    }
    return providersStore.providers[existingIdx];
  }

  // New provider — round-trip through the backend.
  let info;
  try {
    info = await api.addProvider(
      input.name.trim(),
      input.baseUrl.trim(),
      null, // key never flows through the provider-creation IPC
      input.kind,
      input.supportsNativeTools,
    );
  } catch (err) {
    console.error("addProvider failed", err);
    throw err;
  }

  // Store the key BEFORE publishing hasApiKey. If the durable write fails,
  // roll back the just-created provider row so we never leave a keyless
  // provider that looks configured, and surface the error to the UI.
  let hasApiKey = false;
  if (input.apiKey) {
    try {
      // `hasApiKey` mirrors what the backend actually accepted: true only in
      // the Tauri runtime after the keychain write resolved, false in browser
      // fallback where the key was dropped.
      const stored = await setProviderApiKey(info.id, input.apiKey);
      input.apiKey = "";
      hasApiKey = stored;
    } catch (err) {
      console.error("setProviderApiKey failed; rolling back provider", err);
      try {
        await api.removeProvider(info.id);
      } catch (rollbackErr) {
        console.error("rollback removeProvider failed", rollbackErr);
      }
      throw err;
    }
  }

  const provider: Provider = {
    id: info.id,
    name: info.name,
    baseUrl: info.base_url,
    hasApiKey,
    kind: (info.kind as ProviderKind) ?? input.kind,
    isPrivate: info.is_private,
    supportsNativeTools: info.supports_native_tools,
    trustedByName: info.trusted_by_name,
  };
  providersStore.providers.push(provider);
  // First provider added → make it active by default.
  if (providersStore.activeProviderId === null) {
    providersStore.activeProviderId = provider.id;
  }
  persistProviders();
  persistActive();
  return provider;
}

/** Remove a provider only after the backend (or browser mock) confirms it. */
export async function removeProvider(id: string): Promise<void> {
  try {
    await api.removeProvider(id);
  } catch (err) {
    console.error("removeProvider failed", err);
    throw err;
  }
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
