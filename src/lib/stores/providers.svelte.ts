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

/**
 * The user's endpoint selection: a provider AND the model on it, together.
 *
 * Indivisible by construction. There is no way to represent "a provider with
 * no model" or "a model with no provider", in memory or on disk, because a
 * half-pair is what let the app send `model: ""` — or worse, send to a
 * provider the user never chose. A wrong provider is a wrong TRUST ZONE, so
 * this pair may ONLY ever be written by an explicit user choice
 * ({@link setActiveModel}); nothing in this module may infer, default, or
 * slide it to another provider.
 */
export interface ActiveSelection {
  providerId: string;
  model: string;
}

export interface Provider {
  id: string;
  name: string;
  baseUrl: string;
  /** Whether a key has been stored for this provider. The key itself lives
   *  in the OS keychain and NEVER enters the renderer process. */
  hasApiKey: boolean;
  kind: ProviderKind;
  /**
   * Whether the backend classified the endpoint as private/LAN
   * (`Provider::is_private()` — it parses `baseUrl`).
   *
   * This is the RIGHT notion of trust zone — it is the same bit the privacy
   * gate consumes, whereas `kind` is a user-typed label with no enforcement
   * power. But it describes the endpoint's configuration RIGHT NOW, so it must
   * not be used to label a PAST turn: the turn's zone is stamped by the
   * backend and arrives on `ServedBy.zone`. Use this only for statements about
   * the current configuration (e.g. warning about an endpoint the user is
   * about to select), never for the per-turn route badge.
   */
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
/** v2 persists the pair as ONE value (`{providerId, model}` or `null`), so a
 *  half-pair is unrepresentable on disk. v1 stored two independent fields and
 *  could therefore persist a provider with no model. */
const ACTIVE_KEY = "lh.providers.active.v2";
const LEGACY_ACTIVE_KEY = "lh.providers.active.v1";

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

/** Accept a persisted selection ONLY when both halves are real, non-blank
 *  strings. Anything else — a half-pair written by a pre-fix build, a
 *  hand-edited blob, a `null` — is discarded rather than repaired, because
 *  guessing the missing half is exactly the substitution this fix forbids. */
function parseActiveSelection(raw: unknown): ActiveSelection | null {
  if (!raw || typeof raw !== "object") return null;
  const { providerId, model } = raw as Record<string, unknown>;
  if (typeof providerId !== "string" || providerId.trim() === "") return null;
  if (typeof model !== "string" || model.trim() === "") return null;
  return { providerId, model };
}

/** Read the persisted selection, migrating the v1 two-field shape. A v1 blob
 *  that only ever had a provider (the half-pair) is dropped, not adopted. */
function loadActiveSelection(): ActiveSelection | null {
  const rawV2 = localStorage.getItem(ACTIVE_KEY);
  if (rawV2 !== null) return parseActiveSelection(JSON.parse(rawV2));

  const rawV1 = localStorage.getItem(LEGACY_ACTIVE_KEY);
  if (rawV1 === null) return null;
  return parseActiveSelection(JSON.parse(rawV1));
}

function loadFromStorage(): {
  providers: Provider[];
  active: ActiveSelection | null;
} {
  if (typeof localStorage === "undefined") return { providers: [], active: null };
  // Two INDEPENDENT blobs, so two independent try blocks. Sharing one meant a
  // corrupt selection blob threw past the provider parse and discarded the
  // whole provider list — the user's configured endpoints vanishing because a
  // single unrelated key was malformed. Each failure now costs only its own
  // value: unreadable providers → no providers, unreadable selection →
  // disarmed composer (which then refuses to send, the correct fail-closed
  // outcome), never one taking the other with it.
  let providers: Provider[] = [];
  try {
    const rawProv = localStorage.getItem(STORAGE_KEY);
    if (rawProv) providers = migrateProviders(JSON.parse(rawProv));
  } catch {
    providers = [];
  }
  let active: ActiveSelection | null = null;
  try {
    active = loadActiveSelection();
  } catch {
    active = null;
  }
  return { providers, active };
}

const initial = loadFromStorage();

// Runes. `$state` here means a single shared store object that all
// subscribers (the components) read directly. In a `.svelte.ts` module the
// state lives on the module record, so a single import is enough.
export const providersStore = $state({
  providers: initial.providers as Provider[],
  /**
   * The one and only endpoint selection. Write it through
   * {@link setActiveModel} / {@link clearActiveSelection} — never by hand, so
   * every change is traceable to a user action.
   */
  active: initial.active as ActiveSelection | null,
  /**
   * Set when a selection was taken away by something other than the user
   * (the provider vanished from the backend, or was removed), carrying a
   * sentence the UI shows. The old code silently slid the selection to
   * another provider here; the user must instead be TOLD their endpoint is
   * gone, because the alternative is their next message going somewhere they
   * never chose.
   */
  activeSelectionLost: null as string | null,
  /** True while hydrateProviders() is in flight. */
  loading: false as boolean,

  // Read-only views of the pair, for call sites that only need one half.
  // Deliberately getters: assigning either half independently is how the
  // half-pair used to be created, and now simply is not possible.
  get activeProviderId(): string | null {
    return this.active?.providerId ?? null;
  },
  get activeModel(): string | null {
    return this.active?.model ?? null;
  },
});

// Cache of fetched models per provider id — so the provider-list effect that
// drives listing can re-run without re-contacting every endpoint. The installed
// app never substitutes a baked-in model list. Only DISTINCT names are stored
// (see `fetchModels`).
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
    // One value, written whole: either a complete pair or `null`.
    localStorage.setItem(ACTIVE_KEY, JSON.stringify(providersStore.active));
    // Drop the v1 two-field blob once v2 exists, so a downgrade-then-upgrade
    // cycle can't resurrect a half-pair.
    localStorage.removeItem(LEGACY_ACTIVE_KEY);
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

// Same idea for the selection: rewrite any v1 two-field blob into the v2
// single-value shape at load, so the half-pair representation stops existing
// on disk the first time a fixed build runs.
if (
  typeof localStorage !== "undefined" &&
  localStorage.getItem(LEGACY_ACTIVE_KEY) !== null
) {
  persistActive();
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

    // The selection survives hydration ONLY if the provider it names is still
    // configured. If it vanished, the pair is cleared and the loss is
    // surfaced — it is NOT slid onto `merged[0]`.
    //
    // Why that mattered: the backend lists providers `ORDER BY name`, so
    // `merged[0]` is the alphabetically-first provider. Among the quick-add
    // presets that was "Anthropic", which is precisely how a turn the user
    // aimed at their OpenAI endpoint ended up addressed at Anthropic's.
    //
    // And there is deliberately NO "nothing selected → arm the first
    // provider" branch here. Auto-arming is the same substitution wearing a
    // friendlier hat, and it arms a provider with no model.
    const prior = providersStore.active;
    if (prior && !merged.some((p) => p.id === prior.providerId)) {
      const priorName = localById.get(prior.providerId)?.name ?? prior.providerId;
      // Through `clearActiveSelection`, not by hand: this module's own rule is
      // that the pair is only ever written by `setActiveModel` /
      // `clearActiveSelection`, and a hand-written `active = null` here both
      // broke that rule and skipped the `persistActive()` the clear does — the
      // disarm lived only in memory until some later call happened to persist.
      clearActiveSelection(
        `The endpoint you had selected (${priorName}) is no longer configured. Pick a model before sending.`,
      );
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
 * The outcome of listing one provider's models. A discriminated result, not a
 * bare array: the old `catch → return []` made "this endpoint refused us" and
 * "this endpoint has no models" indistinguishable, and the picker then dropped
 * the provider from the popover entirely — leaving a DIFFERENT provider armed
 * and serving every turn, invisibly.
 */
export type ModelListResult =
  | { ok: true; models: string[] }
  | { ok: false; error: string };

function describeError(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  return String(err);
}

/**
 * Fetches the model list for a provider via the backend IPC.
 *
 * Only successes are cached. Pass `{ refresh: true }` to go past the cache.
 * That refresh is NOT a side effect of opening the model picker — opening the
 * picker contacts nothing (see `MainScreen.refreshModelGroups`); it is wired to
 * the picker's explicit "Refresh" button, so a model added on a live endpoint
 * shows up without restarting the app and without silent egress.
 *
 * A failure is REPORTED, never swallowed, and never cached: the caller must be
 * able to tell the user "couldn't list models" instead of quietly hiding the
 * endpoint they picked.
 *
 * **The returned names are DISTINCT.** Every consumer keys a list by model
 * name — the composer's picker (`providerId::name`) and the Settings → Models
 * `<Select>` (`item.value`) — and Svelte throws `each_key_duplicate` on a
 * repeated key, which blanks the whole screen. An endpoint listing the same id
 * twice is perfectly legal on the wire (a proxy fanning out to two upstreams
 * does it routinely), so the guarantee belongs here, where the list is
 * produced, not at each call site: the composer was patched once and Settings
 * still crashed. The backend dedupes too (`models::client::distinct_model_ids`)
 * — this covers the browser-fallback bridge and any endpoint reached by a
 * client that isn't ours.
 */
export async function fetchModels(
  providerId: string,
  opts?: { refresh?: boolean },
): Promise<ModelListResult> {
  if (!opts?.refresh) {
    const cached = modelCache.get(providerId);
    if (cached) return { ok: true, models: cached };
  }
  try {
    // First-seen order kept: the endpoint's own ordering is meaningful, so
    // this only ever drops later repeats.
    const models = [...new Set(await api.listModels(providerId))];
    modelCache.set(providerId, models);
    return { ok: true, models };
  } catch (err) {
    console.error("fetchModels failed", err);
    return { ok: false, error: describeError(err) };
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
    // Repointing the armed provider at a different endpoint moves the turn to
    // a different host — possibly a different trust zone — while the composer
    // still shows the model the user picked from the OLD endpoint. Disarm and
    // say so rather than let the next message follow the URL silently.
    if (
      info.base_url !== existing.baseUrl &&
      providersStore.active?.providerId === id
    ) {
      clearActiveSelection(
        `${info.name}'s endpoint changed to ${info.base_url}, so no model is selected. Pick one before sending.`,
      );
    }
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
  // NO auto-arm, not even for the very first provider. Arming a provider
  // here could only ever arm it WITHOUT a model — which is what sent
  // `model: ""` — and the models on a brand-new endpoint have not been
  // listed yet, so there is nothing honest to arm it with. The user picks in
  // the composer; the composer refuses to send until they do.
  persistProviders();
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
  const removed = providersStore.providers[idx];
  providersStore.providers.splice(idx, 1);
  modelCache.delete(id);
  if (providersStore.active?.providerId === id) {
    // Removing the armed endpoint disarms the composer. It does NOT slide the
    // selection to `providers[0]` — silently re-pointing the composer at a
    // surviving provider is a trust-zone change the user never asked for.
    clearActiveSelection(
      `${removed.name} was removed, so no endpoint is selected. Pick a model before sending.`,
    );
  }
  persistProviders();
}

/**
 * Record the user's explicit endpoint choice — the ONLY way the active pair
 * is ever set.
 *
 * Returns `false` (and changes nothing) when the provider isn't configured or
 * the model is blank. Deliberately not a silent no-op: a caller that believed
 * a selection took, when it didn't, would leave a stale provider armed and
 * send the next turn there.
 */
export function setActiveModel(providerId: string, model: string): boolean {
  const exists = providersStore.providers.some((p) => p.id === providerId);
  if (!exists) {
    console.error(`setActiveModel: no such provider ${providerId}`);
    return false;
  }
  if (model.trim() === "") {
    console.error(`setActiveModel: refusing to arm ${providerId} with no model`);
    return false;
  }
  providersStore.active = { providerId, model };
  providersStore.activeSelectionLost = null;
  persistActive();
  return true;
}

/** Disarm the composer. `reason` is shown to the user when the clearing was
 *  not their direct doing (a provider vanishing); pass nothing for a plain
 *  user-initiated clear. */
export function clearActiveSelection(reason: string | null = null): void {
  providersStore.active = null;
  providersStore.activeSelectionLost = reason;
  persistActive();
}

/** Reset everything — used in tests / future "clear all" UI. */
export function resetProviders(): void {
  providersStore.providers = [];
  modelCache.clear();
  persistProviders();
  // Same rule as everywhere else in this module: the pair is written through
  // `clearActiveSelection`, never by hand. (No reason — this clear IS the
  // caller's own doing.)
  clearActiveSelection();
}

// Quick lookup for the UI; pure derived data off the runes state.
export function getProvider(id: string | null): Provider | null {
  if (!id) return null;
  return providersStore.providers.find((p) => p.id === id) ?? null;
}
