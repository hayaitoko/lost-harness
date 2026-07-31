/// <reference types="vitest" />

/**
 * Provider store tests — security-sensitive provider mutations including
 * add/remove/update, API-key handling, localStorage persistence, the model
 * cache, and the hydration merge logic.
 *
 * Most tests run against the browser fallback (no Tauri shell). The tauri.ts
 * module auto-detects the absence of __TAURI_INTERNALS__ and routes
 * through its browser-mock functions, which persist to localStorage.
 * This tests both the store's logic AND the browser-fallback correctness,
 * since the store calls api.* which routes through browser*.
 *
 * H-01 (P04): the "provider key IPC" block at the bottom instead simulates the
 * installed app — it stubs `window.__TAURI_INTERNALS__` and mocks
 * `@tauri-apps/api/core`'s `invoke` (which both tauri.ts and the store import)
 * so the `set_provider_api_key` one-shot IPC can be made to succeed or fail on
 * demand. Those tests pin the invariant that `hasApiKey` is true ONLY after a
 * durable keychain write, and that no plaintext key ever reaches localStorage.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import type { ProviderInfo } from "$lib/api/tauri";
import {
  providersStore,
  hydrateProviders,
  addProvider,
  removeProvider,
  resetProviders,
  fetchModels,
  setActiveModel,
  clearActiveSelection,
  getProvider,
} from "$lib/stores/providers.svelte";

// Mocked for the Tauri-runtime block below. Harmless for the browser-fallback
// blocks: with __TAURI_INTERNALS__ undefined, `invoke` is never reached.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
import { invoke } from "@tauri-apps/api/core";

// ── Helpers ─────────────────────────────────────────────────────────────────

function seedLocalStorage(providers: ProviderInfo[]) {
  localStorage.setItem("lh.providers.browser.v1", JSON.stringify(providers));
}

/** Every key/value currently in localStorage, flattened — used to assert that
 *  a plaintext secret is nowhere on disk, not merely absent from one key. */
function dumpLocalStorage(): string {
  let out = "";
  for (let i = 0; i < localStorage.length; i++) {
    const k = localStorage.key(i)!;
    out += `${k}=${localStorage.getItem(k)}\n`;
  }
  return out;
}

// ── Tests ───────────────────────────────────────────────────────────────────

describe("providers store — initial state", () => {
  it("starts with empty providers from empty localStorage", () => {
    expect(providersStore.providers).toEqual([]);
    expect(providersStore.activeProviderId).toBeNull();
    expect(providersStore.activeModel).toBeNull();
    expect(providersStore.loading).toBe(false);
  });

  it("does not adopt seeded browser-fallback storage until hydrateProviders() runs", async () => {
    resetProviders();
    seedLocalStorage([
      { id: "p1", name: "LocalAI", base_url: "http://localhost:8080", kind: "local", is_private: true, trusted_by_name: false, supports_native_tools: false },
    ]);

    // Seeding the fallback store alone must not mutate the live store — the
    // module read localStorage once at import time and never polls it.
    expect(providersStore.providers).toEqual([]);

    await hydrateProviders();

    // After hydration the snake_case ProviderInfo is mapped to camelCase
    // Provider. H-01: the frontend model carries no `apiKey` field at all —
    // only the `hasApiKey` boolean, which the backend did not report, so false.
    expect(providersStore.providers).toHaveLength(1);
    expect(providersStore.providers[0]).toMatchObject({
      id: "p1",
      name: "LocalAI",
      baseUrl: "http://localhost:8080",
      kind: "local",
      isPrivate: true,
      supportsNativeTools: false,
      trustedByName: false,
      hasApiKey: false,
    });
    expect(providersStore.providers[0]).not.toHaveProperty("apiKey");
  });
});

describe("providers store — addProvider", () => {
  beforeEach(() => {
    resetProviders();
  });

  it("adds a local provider via browser fallback and updates the store", async () => {
    const p = await addProvider({
      name: "Ollama",
      baseUrl: "http://localhost:11434",
      apiKey: "",
      kind: "local",
      supportsNativeTools: false,
    });

    expect(p.id).toBeTruthy();
    expect(p.name).toBe("Ollama");
    expect(p.kind).toBe("local");
    expect(p.isPrivate).toBe(true);
    expect(providersStore.providers).toHaveLength(1);
    // Adding a provider does NOT arm it — see the auto-arm test below.
    expect(providersStore.active).toBeNull();
  });

  it("adds a cloud provider with isPrivate=false", async () => {
    const p = await addProvider({
      name: "OpenAI",
      baseUrl: "https://api.openai.com/v1",
      apiKey: "sk-test",
      kind: "cloud",
      supportsNativeTools: true,
    });

    expect(p.isPrivate).toBe(false);
    expect(p.kind).toBe("cloud");
  });

  it("browser fallback has no keychain, so a new provider's key is NOT reported as stored", async () => {
    // H-01: without a Tauri shell the plaintext key is dropped on the floor.
    // Reporting hasApiKey=true here would tell the user a credential is saved
    // when none exists anywhere.
    const input = {
      name: "OpenAI",
      baseUrl: "https://api.openai.com/v1",
      apiKey: "sk-dev-mode-secret",
      kind: "cloud" as const,
      supportsNativeTools: true,
    };
    const p = await addProvider(input);

    expect(p.hasApiKey).toBe(false);
    expect(providersStore.providers[0].hasApiKey).toBe(false);
    expect(JSON.parse(localStorage.getItem("lh.providers.v1")!)[0].hasApiKey).toBe(false);
    expect(input.apiKey).toBe("");
    expect(dumpLocalStorage()).not.toContain("sk-dev-mode-secret");
  });

  it("does NOT auto-arm the first provider — an armed provider with no model is what sent model:''", async () => {
    expect(providersStore.active).toBeNull();
    await addProvider({
      name: "Test",
      baseUrl: "http://localhost:8080",
      apiKey: "",
      kind: "local",
      supportsNativeTools: false,
    });
    // The old code armed this provider here, with no model — so the composer
    // looked ready and sent `model: ""`. The endpoint is chosen by the user
    // in the picker, or not at all.
    expect(providersStore.active).toBeNull();
    expect(providersStore.activeProviderId).toBeNull();
    expect(providersStore.activeModel).toBeNull();
  });

  it("persists to localStorage", async () => {
    await addProvider({
      name: "Persist",
      baseUrl: "http://localhost:8080",
      apiKey: "",
      kind: "local",
      supportsNativeTools: false,
    });

    const raw = localStorage.getItem("lh.providers.browser.v1");
    expect(raw).toBeTruthy();
    const parsed = JSON.parse(raw!);
    expect(parsed).toHaveLength(1);
    expect(parsed[0].name).toBe("Persist");
  });
});

describe("providers store — removeProvider", () => {
  beforeEach(async () => {
    resetProviders();
    await addProvider({ name: "P1", baseUrl: "http://localhost:1", apiKey: "", kind: "local", supportsNativeTools: false });
    await addProvider({ name: "P2", baseUrl: "http://localhost:2", apiKey: "", kind: "local", supportsNativeTools: false });
  });

  it("removes a provider from the store and localStorage", async () => {
    const firstId = providersStore.providers[0].id;
    await removeProvider(firstId);

    expect(providersStore.providers).toHaveLength(1);
    expect(providersStore.providers[0].id).not.toBe(firstId);

    const raw = localStorage.getItem("lh.providers.browser.v1");
    const parsed = JSON.parse(raw!);
    expect(parsed).toHaveLength(1);
  });

  it("CLEARS the selection when the active provider is removed — it must not slide to another provider", async () => {
    // This test previously asserted the opposite ("switches active"), blessing
    // the exact bug: removing the armed endpoint silently re-pointed the
    // composer at providers[0]. Sliding a selection is a trust-zone change the
    // user never made — the next message would go to a different vendor.
    const ids = providersStore.providers.map((p) => p.id);
    setActiveModel(ids[0], "llama3");

    await removeProvider(ids[0]);

    expect(providersStore.active).toBeNull();
    expect(providersStore.activeProviderId).toBeNull();
    expect(providersStore.activeModel).toBeNull();
    // The surviving provider is still there — it just isn't armed.
    expect(providersStore.providers.map((p) => p.id)).toEqual([ids[1]]);
    // ...and the user is told, rather than left to discover it by sending.
    expect(providersStore.activeSelectionLost).toMatch(/removed/i);
  });

  it("leaves an unrelated selection alone when a different provider is removed", async () => {
    const ids = providersStore.providers.map((p) => p.id);
    setActiveModel(ids[1], "mistral");

    await removeProvider(ids[0]);

    expect(providersStore.active).toEqual({ providerId: ids[1], model: "mistral" });
    expect(providersStore.activeSelectionLost).toBeNull();
  });

  it("sets active to null when the last provider is removed", async () => {
    const ids = providersStore.providers.map((p) => p.id);
    await removeProvider(ids[0]);
    await removeProvider(ids[1]);

    expect(providersStore.providers).toHaveLength(0);
    expect(providersStore.activeProviderId).toBeNull();
  });

  it("is a no-op on a non-existent id", async () => {
    const before = providersStore.providers.length;
    // removeProvider calls api.removeProvider (browser mock) which filters by id
    await removeProvider("non-existent");
    expect(providersStore.providers).toHaveLength(before);
  });
});

describe("providers store — apiKey security (blank = keep stored)", () => {
  // Preserves P22's intent — "blank key on edit must not clear the stored
  // credential" — restated against the post-H-01 model, where the frontend
  // holds only `hasApiKey` and the secret itself lives in the OS keychain.
  beforeEach(async () => {
    resetProviders();
    // Manually set a provider that already has a key in the keychain.
    providersStore.providers = [{
      id: "test-id",
      name: "Test",
      baseUrl: "http://localhost:8080",
      hasApiKey: true,
      kind: "custom",
      isPrivate: true,
      supportsNativeTools: false,
      trustedByName: false,
    }];
    setActiveModel("test-id", "some-model");
  });

  it("updateProvider with apiKey='' keeps the stored key (hasApiKey stays true)", async () => {
    // Re-add with the same id and blank key — the update branch must carry the
    // previous hasApiKey through instead of resetting it to false.
    const updated = await addProvider({
      id: "test-id",
      name: "Test",
      baseUrl: "http://localhost:8080",
      apiKey: "",
      kind: "custom",
      supportsNativeTools: false,
    });

    expect(updated.hasApiKey).toBe(true);
    expect(providersStore.providers[0].hasApiKey).toBe(true);
  });

  it("never lets a plaintext key reach the frontend model or localStorage", async () => {
    const input = {
      id: "test-id",
      name: "Test",
      baseUrl: "http://localhost:8080",
      apiKey: "new-secret-key",
      kind: "custom" as const,
      supportsNativeTools: false,
    };
    const updated = await addProvider(input);

    expect(updated).not.toHaveProperty("apiKey");
    expect(dumpLocalStorage()).not.toContain("new-secret-key");
    // The store consumed the plaintext and blanked the caller's copy.
    expect(input.apiKey).toBe("");
  });

  it("browser fallback has no keychain, so a new key must NOT be reported as stored", async () => {
    // A keyless provider + a new key, with no Tauri shell: the key is dropped,
    // so claiming hasApiKey=true would be the same lie as the swallowed-IPC bug.
    providersStore.providers[0].hasApiKey = false;

    const updated = await addProvider({
      id: "test-id",
      name: "Test",
      baseUrl: "http://localhost:8080",
      apiKey: "dev-mode-key",
      kind: "custom",
      supportsNativeTools: false,
    });

    expect(updated.hasApiKey).toBe(false);
    expect(providersStore.providers[0].hasApiKey).toBe(false);
  });
});

describe("providers store — setActiveModel", () => {
  beforeEach(async () => {
    resetProviders();
    await addProvider({ name: "P1", baseUrl: "http://localhost:1", apiKey: "", kind: "local", supportsNativeTools: false });
  });

  it("sets the active provider and model together, and reports success", () => {
    const id = providersStore.providers[0].id;
    expect(setActiveModel(id, "gpt-4")).toBe(true);

    expect(providersStore.active).toEqual({ providerId: id, model: "gpt-4" });
    expect(providersStore.activeProviderId).toBe(id);
    expect(providersStore.activeModel).toBe("gpt-4");
  });

  it("REPORTS FALSE for an unknown provider id instead of silently no-opping", () => {
    // The silent no-op was its own bug: the caller believed the selection had
    // moved while the PREVIOUS endpoint stayed armed and served the next turn.
    const known = providersStore.providers[0].id;
    setActiveModel(known, "original");

    expect(setActiveModel("unknown-id", "gpt-4")).toBe(false);

    expect(providersStore.active).toEqual({ providerId: known, model: "original" });
  });

  it("refuses to arm a provider with a blank model", () => {
    const id = providersStore.providers[0].id;

    expect(setActiveModel(id, "")).toBe(false);
    expect(setActiveModel(id, "   ")).toBe(false);

    // No half-pair was created — this is the state that sent `model: ""`.
    expect(providersStore.active).toBeNull();
  });

  it("clears the 'your endpoint went away' notice once the user picks again", () => {
    const id = providersStore.providers[0].id;
    clearActiveSelection("the endpoint vanished");
    expect(providersStore.activeSelectionLost).toBe("the endpoint vanished");

    setActiveModel(id, "gpt-4");

    expect(providersStore.activeSelectionLost).toBeNull();
  });

  it("persists the selection as ONE value, so a half-pair is unrepresentable", () => {
    const id = providersStore.providers[0].id;
    setActiveModel(id, "claude-3");

    const raw = localStorage.getItem("lh.providers.active.v2");
    expect(raw).toBeTruthy();
    expect(JSON.parse(raw!)).toEqual({ providerId: id, model: "claude-3" });

    // Cleared state persists as a single null, not as two independent fields
    // one of which could survive alone.
    clearActiveSelection();
    expect(localStorage.getItem("lh.providers.active.v2")).toBe("null");
  });
});

describe("providers store — resetProviders", () => {
  it("clears everything including localStorage and modelCache", async () => {
    const p = await addProvider({ name: "P1", baseUrl: "http://localhost:1", apiKey: "", kind: "local", supportsNativeTools: false });
    setActiveModel(p.id, "some-model");

    resetProviders();

    expect(providersStore.providers).toEqual([]);
    expect(providersStore.active).toBeNull();
    expect(providersStore.activeProviderId).toBeNull();
    expect(providersStore.activeModel).toBeNull();
    // localStorage should be cleared (persisted as empty array / null entry)
    expect(localStorage.getItem("lh.providers.v1")).toBe("[]");
    expect(localStorage.getItem("lh.providers.active.v2")).toBe("null");
  });
});

describe("providers store — getProvider", () => {
  beforeEach(async () => {
    resetProviders();
    await addProvider({ name: "Finder", baseUrl: "http://localhost:1", apiKey: "", kind: "local", supportsNativeTools: false });
  });

  it("returns the provider by id", () => {
    const id = providersStore.providers[0].id;
    const found = getProvider(id);
    expect(found).not.toBeNull();
    expect(found!.name).toBe("Finder");
  });

  it("returns null for null id", () => {
    expect(getProvider(null)).toBeNull();
  });

  it("returns null for unknown id", () => {
    expect(getProvider("nobody")).toBeNull();
  });
});

describe("providers store — fetchModels", () => {
  beforeEach(() => {
    resetProviders();
  });

  it("returns cached models on second call", async () => {
    // First call: browser fallback returns ["default"]
    const first = await fetchModels("any-id");
    expect(first).toEqual({ ok: true, models: ["default"] });

    const second = await fetchModels("any-id");
    expect(second).toEqual({ ok: true, models: ["default"] });
  });

});

describe("providers store — fetchModels failures and refresh (Tauri runtime, mocked)", () => {
  const invokeMock = vi.mocked(invoke);
  /** What the fake `list_models` IPC should do on its next call. */
  let listModelsImpl: () => Promise<string[]>;
  let listModelsCalls = 0;

  beforeEach(() => {
    listModelsCalls = 0;
    listModelsImpl = async () => ["default"];
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    invokeMock.mockReset();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_models") {
        listModelsCalls += 1;
        return listModelsImpl();
      }
      if (cmd === "list_providers") return [];
      throw new Error(`unexpected IPC command: ${cmd}`);
    });
    resetProviders();
  });

  afterEach(() => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined;
    invokeMock.mockReset();
  });

  it("reports a listing failure instead of swallowing it into an empty list", async () => {
    // The swallowed `catch → return []` is why a provider whose /models call
    // failed vanished from the picker entirely, leaving a DIFFERENT provider
    // armed and serving every turn. "The endpoint refused us" and "the
    // endpoint has no models" are different facts with different fixes, and
    // must stay distinguishable.
    listModelsImpl = async () => {
      throw new Error("401 Unauthorized");
    };

    const result = await fetchModels("broken-provider");

    expect(result).toEqual({ ok: false, error: "401 Unauthorized" });
  });

  it("never caches a failure, and can refresh past a cached success", async () => {
    listModelsImpl = async () => {
      throw new Error("connection refused");
    };
    expect(await fetchModels("p")).toEqual({ ok: false, error: "connection refused" });

    // The failure was NOT cached — the next attempt really re-asks and wins.
    listModelsImpl = async () => ["a"];
    expect(await fetchModels("p")).toEqual({ ok: true, models: ["a"] });
    expect(listModelsCalls).toBe(2);

    // A success IS cached: no third call to the endpoint.
    expect(await fetchModels("p")).toEqual({ ok: true, models: ["a"] });
    expect(listModelsCalls).toBe(2);

    // ...until an explicit refresh. That refresh is the picker's "Refresh"
    // BUTTON, not a side effect of the picker opening — opening it contacts
    // nothing (see picker-egress.test.ts). The point of having it at all is
    // that a model added on a live endpoint isn't invisible until the app
    // restarts.
    listModelsImpl = async () => ["a", "b-just-added"];
    expect(await fetchModels("p", { refresh: true })).toEqual({
      ok: true,
      models: ["a", "b-just-added"],
    });
    expect(listModelsCalls).toBe(3);
  });
});

describe("providers store — hydrateProviders merge logic", () => {
  beforeEach(() => {
    resetProviders();
  });

  it("loads providers from browser fallback via api.listProviders", async () => {
    // Seed localStorage with a provider so browserListProviders returns it
    seedLocalStorage([
      { id: "b1", name: "Browser", base_url: "http://localhost:1234", kind: "local", is_private: true, trusted_by_name: false, supports_native_tools: false },
    ]);

    await hydrateProviders();

    expect(providersStore.providers).toHaveLength(1);
    expect(providersStore.providers[0].name).toBe("Browser");
    // Ensure loading flag resets
    expect(providersStore.loading).toBe(false);
  });

  it("preserves the local hasApiKey flag when merging with a remote list that omits it", async () => {
    // Simulate the Tauri path: the backend's ProviderInfo carries no
    // has_api_key, so hydration must not downgrade a known-configured
    // provider to "no key" (which would tell the user their key vanished).
    providersStore.providers = [{
      id: "b1",
      name: "Browser",
      baseUrl: "http://localhost:1234",
      hasApiKey: true,
      kind: "local",
      isPrivate: true,
      supportsNativeTools: false,
      trustedByName: false,
    }];

    seedLocalStorage([
      { id: "b1", name: "Browser", base_url: "http://localhost:1234", kind: "local", is_private: true, trusted_by_name: false, supports_native_tools: false },
    ]);

    await hydrateProviders();

    expect(providersStore.loading).toBe(false);
    expect(providersStore.providers).toHaveLength(1);
    expect(providersStore.providers[0].id).toBe("b1");
    expect(providersStore.providers[0].hasApiKey).toBe(true);
  });

  it("handles hydration failure gracefully", async () => {
    // Corrupt localStorage to force a parse error
    localStorage.setItem("lh.providers.browser.v1", "not-json");

    // Should not throw
    await hydrateProviders();

    expect(providersStore.loading).toBe(false);
  });

  it("does NOT auto-arm a provider when nothing was selected", async () => {
    // The auto-arm branch is the precise mechanism of the reported bug. The
    // backend lists providers `ORDER BY name`, so "the first provider" is the
    // ALPHABETICALLY first one — which among the quick-add presets was
    // "Anthropic". That is how turns aimed at an OpenAI endpoint ended up
    // addressed at Anthropic's.
    seedLocalStorage([
      { id: "anthropic", name: "Anthropic", base_url: "https://api.anthropic.com/v1", kind: "cloud", is_private: false, trusted_by_name: false, supports_native_tools: false },
      { id: "openai", name: "OpenAI", base_url: "https://api.openai.com/v1", kind: "cloud", is_private: false, trusted_by_name: false, supports_native_tools: false },
    ]);
    clearActiveSelection();

    await hydrateProviders();

    expect(providersStore.providers).toHaveLength(2);
    expect(providersStore.active).toBeNull();
    expect(providersStore.activeProviderId).toBeNull();
  });

  it("keeps a still-valid selection across hydration", async () => {
    providersStore.providers = [{
      id: "b1",
      name: "Browser",
      baseUrl: "http://localhost:1234",
      hasApiKey: false,
      kind: "local",
      isPrivate: true,
      supportsNativeTools: false,
      trustedByName: false,
    }];
    setActiveModel("b1", "llama3");
    seedLocalStorage([
      { id: "b1", name: "Browser", base_url: "http://localhost:1234", kind: "local", is_private: true, trusted_by_name: false, supports_native_tools: false },
    ]);

    await hydrateProviders();

    expect(providersStore.active).toEqual({ providerId: "b1", model: "llama3" });
    expect(providersStore.activeSelectionLost).toBeNull();
  });

});

describe("providers store — hydrateProviders in the installed app (Tauri runtime, mocked)", () => {
  // The installed app drops local cache entries the backend doesn't know
  // about, so this is the only runtime where a persisted provider can truly
  // VANISH under the user — the case that produced the reported bug.
  const invokeMock = vi.mocked(invoke);
  let remoteProviders: ProviderInfo[] = [];

  beforeEach(() => {
    remoteProviders = [];
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    invokeMock.mockReset();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_providers") return remoteProviders;
      if (cmd === "list_models") return ["default"];
      throw new Error(`unexpected IPC command: ${cmd}`);
    });
    resetProviders();
  });

  afterEach(() => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined;
    invokeMock.mockReset();
  });

  /** Put a provider in the store as though a previous session had it. */
  function seedStoreProvider(id: string, name: string) {
    providersStore.providers = [
      ...providersStore.providers,
      {
        id,
        name,
        baseUrl: `http://${id}.example/v1`,
        hasApiKey: false,
        kind: "custom",
        isPrivate: false,
        supportsNativeTools: false,
        trustedByName: false,
      },
    ];
  }

  it("CLEARS the selection when the persisted provider is gone — it must not slide to providers[0]", async () => {
    seedStoreProvider("my-openai", "My OpenAI box");
    setActiveModel("my-openai", "qwen3-32b");
    // The backend no longer has it, and returns the alphabetically-first
    // provider that the old code would have silently slid onto. The backend
    // orders `ORDER BY name`, which is why "Anthropic" was always first.
    remoteProviders = [
      { id: "anthropic", name: "Anthropic", base_url: "https://api.anthropic.com/v1", kind: "cloud", is_private: false, trusted_by_name: false, supports_native_tools: false },
    ];

    await hydrateProviders();

    expect(providersStore.active).toBeNull();
    // Emphatically NOT "anthropic".
    expect(providersStore.activeProviderId).not.toBe("anthropic");
    // ...and the user is TOLD their endpoint went away, instead of silently
    // getting a different vendor — a different TRUST ZONE — on the next turn.
    expect(providersStore.activeSelectionLost).toContain("My OpenAI box");
  });

  it("does not auto-arm the alphabetically-first provider when nothing is selected", async () => {
    remoteProviders = [
      { id: "anthropic", name: "Anthropic", base_url: "https://api.anthropic.com/v1", kind: "cloud", is_private: false, trusted_by_name: false, supports_native_tools: false },
      { id: "openai", name: "OpenAI", base_url: "https://api.openai.com/v1", kind: "cloud", is_private: false, trusted_by_name: false, supports_native_tools: false },
    ];

    await hydrateProviders();

    expect(providersStore.providers).toHaveLength(2);
    expect(providersStore.active).toBeNull();
  });

  it("can never leave activeProviderId and activeModel owned by different providers", async () => {
    // The invariant, stated directly: whatever hydration does to the
    // selection, the two halves always describe ONE configured provider, or
    // both are null. There is no reachable state in between.
    const decoy: ProviderInfo = { id: "anthropic", name: "Anthropic", base_url: "https://api.anthropic.com/v1", kind: "cloud", is_private: false, trusted_by_name: false, supports_native_tools: false };
    const kept: ProviderInfo = { id: "kept", name: "Kept", base_url: "http://kept.example/v1", kind: "custom", is_private: false, trusted_by_name: false, supports_native_tools: false };

    const scenarios: ProviderInfo[][] = [
      [], // everything vanished
      [decoy], // the armed one vanished, a different one remains
      [decoy, kept], // the armed one survives alongside another
      [kept], // only the armed one remains
    ];

    for (const remote of scenarios) {
      resetProviders();
      seedStoreProvider("kept", "Kept");
      setActiveModel("kept", "the-model");
      remoteProviders = remote;

      await hydrateProviders();

      const { activeProviderId, activeModel } = providersStore;
      // Both set, or both null. Never one without the other.
      expect(activeProviderId === null).toBe(activeModel === null);
      if (activeProviderId !== null) {
        // ...and the provider it names is really configured, and it is the
        // one the model was actually chosen from — never a substitute.
        expect(providersStore.providers.some((p) => p.id === activeProviderId)).toBe(true);
        expect(activeProviderId).toBe("kept");
        expect(activeModel).toBe("the-model");
      }
    }
  });
});

describe("providers store — persisted selection shape", () => {
  afterEach(() => {
    vi.resetModules();
  });

  it("migrates a complete v1 pair into the v2 single value", async () => {
    localStorage.setItem(
      "lh.providers.active.v1",
      JSON.stringify({ providerId: "p1", model: "gpt-4" }),
    );

    vi.resetModules();
    const fresh = await import("$lib/stores/providers.svelte");

    expect(fresh.providersStore.active).toEqual({ providerId: "p1", model: "gpt-4" });
    // The v1 key is gone, so the two-field representation stops existing on
    // disk the first time a fixed build runs.
    expect(localStorage.getItem("lh.providers.active.v1")).toBeNull();
    expect(JSON.parse(localStorage.getItem("lh.providers.active.v2")!)).toEqual({
      providerId: "p1",
      model: "gpt-4",
    });
  });

  it("DISCARDS a persisted v1 half-pair rather than repairing it", async () => {
    // A provider armed with no model is exactly what a pre-fix build could
    // write, and exactly what sent `model: ""`. Guessing the missing half
    // would be the substitution this whole fix forbids.
    localStorage.setItem(
      "lh.providers.active.v1",
      JSON.stringify({ providerId: "p1", model: null }),
    );

    vi.resetModules();
    const fresh = await import("$lib/stores/providers.svelte");

    expect(fresh.providersStore.active).toBeNull();
    expect(localStorage.getItem("lh.providers.active.v2")).toBe("null");
  });

  it("ignores a corrupt persisted selection instead of half-adopting it", async () => {
    localStorage.setItem("lh.providers.active.v2", "{not json");

    vi.resetModules();
    const fresh = await import("$lib/stores/providers.svelte");

    expect(fresh.providersStore.active).toBeNull();
  });

  it("a corrupt selection blob does not take the PROVIDER LIST down with it", async () => {
    // The two blobs are independent values under independent keys, and used to
    // be parsed inside one shared try: a throw from the selection parse escaped
    // past the already-parsed providers and returned the `empty` fallback, so
    // one malformed key made every configured endpoint vanish. Each failure
    // must cost only its own value.
    localStorage.setItem(
      "lh.providers.v1",
      JSON.stringify([
        {
          id: "keep-me",
          name: "My box",
          baseUrl: "http://10.0.0.5:8000/v1",
          hasApiKey: false,
          kind: "custom",
          isPrivate: true,
          supportsNativeTools: true,
          trustedByName: false,
        },
      ]),
    );
    localStorage.setItem("lh.providers.active.v2", "{not json");

    vi.resetModules();
    const fresh = await import("$lib/stores/providers.svelte");

    expect(fresh.providersStore.providers.map((p) => p.id)).toEqual(["keep-me"]);
    // The selection alone was lost — the composer is disarmed (fail closed),
    // not repointed at the surviving provider.
    expect(fresh.providersStore.active).toBeNull();
  });
});

// ── H-01 (P04) ──────────────────────────────────────────────────────────────

describe("providers store — legacy plaintext-key scrub (H-01)", () => {
  // The store module reads and rewrites localStorage at import time, so these
  // tests must seed storage and then re-import the module.
  afterEach(() => {
    vi.resetModules();
  });

  it("removes a plaintext apiKey persisted by a pre-fix build on module load", async () => {
    localStorage.setItem(
      "lh.providers.v1",
      JSON.stringify([
        {
          id: "legacy-keyed",
          name: "OpenAI",
          baseUrl: "https://api.openai.com/v1",
          apiKey: "sk-legacy-plaintext-secret",
          kind: "cloud",
          isPrivate: false,
          supportsNativeTools: true,
          trustedByName: false,
        },
        {
          id: "legacy-keyless",
          name: "Ollama",
          baseUrl: "http://localhost:11434",
          apiKey: "",
          kind: "local",
          isPrivate: true,
          supportsNativeTools: false,
          trustedByName: false,
        },
      ]),
    );

    vi.resetModules();
    const fresh = await import("$lib/stores/providers.svelte");

    // The on-disk payload must have been rewritten without the secret. This is
    // asserted on the raw string so a nested/renamed leak still trips it.
    const raw = localStorage.getItem("lh.providers.v1")!;
    expect(raw).not.toContain("sk-legacy-plaintext-secret");
    expect(raw).not.toContain("apiKey");
    expect(dumpLocalStorage()).not.toContain("sk-legacy-plaintext-secret");

    // The metadata survives, translated to the boolean.
    const parsed = JSON.parse(raw) as Array<Record<string, unknown>>;
    expect(parsed).toHaveLength(2);
    expect(parsed[0]).toMatchObject({ id: "legacy-keyed", hasApiKey: true });
    expect(parsed[1]).toMatchObject({ id: "legacy-keyless", hasApiKey: false });

    // ...and the in-memory model carries no secret either.
    const loaded = fresh.providersStore.providers;
    expect(loaded).toHaveLength(2);
    expect(loaded[0]).not.toHaveProperty("apiKey");
    expect(loaded[0].hasApiKey).toBe(true);
    expect(loaded[1].hasApiKey).toBe(false);
  });

  it("clears an unparseable legacy payload instead of leaving it on disk", async () => {
    localStorage.setItem("lh.providers.v1", "{not json at all sk-corrupt-secret");

    vi.resetModules();
    const fresh = await import("$lib/stores/providers.svelte");

    expect(localStorage.getItem("lh.providers.v1")).toBe("[]");
    expect(dumpLocalStorage()).not.toContain("sk-corrupt-secret");
    expect(fresh.providersStore.providers).toEqual([]);
  });
});

describe("providers store — provider key IPC (Tauri runtime, mocked)", () => {
  const invokeMock = vi.mocked(invoke);

  /** Key payloads the fake backend was asked to store. */
  let keyCalls: Array<{ provider_id: string; api_key: string }> = [];
  /** When true, the keychain write rejects (P05's command returning Err). */
  let keyIpcFails = false;

  beforeEach(() => {
    keyCalls = [];
    keyIpcFails = false;
    // Pretend we are inside the installed app so tauri.ts and the store both
    // take their `invoke` paths instead of the browser fallback.
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    invokeMock.mockReset();
    invokeMock.mockImplementation(async (cmd: string, payload?: unknown) => {
      const args = (payload as { args?: Record<string, any> } | undefined)?.args;
      switch (cmd) {
        case "add_provider":
          return {
            id: "srv-new",
            name: args!.name,
            base_url: args!.base_url,
            kind: args!.kind,
            is_private: false,
            trusted_by_name: false,
            supports_native_tools: args!.supports_native_tools,
          };
        case "update_provider":
          return {
            id: args!.id,
            name: args!.name,
            base_url: args!.base_url,
            kind: args!.kind,
            is_private: false,
            trusted_by_name: false,
            supports_native_tools: args!.supports_native_tools,
          };
        case "set_provider_api_key":
          keyCalls.push(args as { provider_id: string; api_key: string });
          if (keyIpcFails) throw new Error("keychain write refused");
          return null;
        case "remove_provider":
          return true;
        case "list_providers":
          return [];
        case "list_models":
          return ["default"];
        default:
          throw new Error(`unexpected IPC command: ${cmd}`);
      }
    });
    resetProviders();
  });

  afterEach(() => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined;
    invokeMock.mockReset();
  });

  /** Seed an already-persisted provider without going through the IPC. */
  function seedStoredProvider(hasApiKey: boolean) {
    providersStore.providers = [
      {
        id: "srv-9",
        name: "Cloudy",
        baseUrl: "https://api.example.com/v1",
        hasApiKey,
        kind: "cloud",
        isPrivate: false,
        supportsNativeTools: true,
        trustedByName: false,
      },
    ];
    setActiveModel("srv-9", "some-model");
  }

  it("add: reports hasApiKey only after the key IPC resolves, and leaks no plaintext", async () => {
    const input = {
      name: "OpenAI",
      baseUrl: "https://api.openai.com/v1",
      apiKey: "sk-live-secret-value",
      kind: "cloud" as const,
      supportsNativeTools: true,
    };

    const created = await addProvider(input);

    // The key went to the dedicated one-shot command, keyed to the id the
    // backend just assigned.
    expect(keyCalls).toEqual([
      { provider_id: "srv-new", api_key: "sk-live-secret-value" },
    ]);
    // ...and NOT through the provider-creation command.
    expect(invokeMock).toHaveBeenCalledWith("add_provider", {
      args: expect.objectContaining({ api_key: null }),
    });

    expect(created.hasApiKey).toBe(true);
    expect(created).not.toHaveProperty("apiKey");
    // The caller's plaintext buffer was blanked, and nothing hit the disk.
    expect(input.apiKey).toBe("");
    expect(dumpLocalStorage()).not.toContain("sk-live-secret-value");
  });

  it("add: a rejected key IPC surfaces the error and rolls the provider back", async () => {
    keyIpcFails = true;
    const input = {
      name: "OpenAI",
      baseUrl: "https://api.openai.com/v1",
      apiKey: "sk-doomed-secret-value",
      kind: "cloud" as const,
      supportsNativeTools: true,
    };

    // Not swallowed — the UI must be able to show this.
    await expect(addProvider(input)).rejects.toThrow("keychain write refused");

    // The half-created backend row was deleted again.
    expect(invokeMock).toHaveBeenCalledWith("remove_provider", { id: "srv-new" });
    // No provider is left behind, in memory or on disk, so nothing can claim
    // to be configured.
    expect(providersStore.providers).toEqual([]);
    expect(JSON.parse(localStorage.getItem("lh.providers.v1")!)).toEqual([]);
    expect(providersStore.activeProviderId).toBeNull();
    expect(dumpLocalStorage()).not.toContain("sk-doomed-secret-value");
  });

  it("add: with no key supplied the key IPC is never called and hasApiKey is false", async () => {
    const created = await addProvider({
      name: "Ollama",
      baseUrl: "http://localhost:11434",
      apiKey: "",
      kind: "local",
      supportsNativeTools: false,
    });

    expect(keyCalls).toEqual([]);
    expect(created.hasApiKey).toBe(false);
  });

  it("rotate: a successful key IPC flips hasApiKey to true and persists it", async () => {
    seedStoredProvider(false);

    const updated = await addProvider({
      id: "srv-9",
      name: "Cloudy",
      baseUrl: "https://api.example.com/v1",
      apiKey: "sk-rotated-secret-value",
      kind: "cloud",
      supportsNativeTools: true,
    });

    expect(keyCalls).toEqual([
      { provider_id: "srv-9", api_key: "sk-rotated-secret-value" },
    ]);
    expect(updated.hasApiKey).toBe(true);
    expect(providersStore.providers[0].hasApiKey).toBe(true);

    const persisted = JSON.parse(localStorage.getItem("lh.providers.v1")!);
    expect(persisted[0].hasApiKey).toBe(true);
    expect(dumpLocalStorage()).not.toContain("sk-rotated-secret-value");
  });

  it("rotate: a rejected key IPC must NOT claim the key was stored", async () => {
    // The regression this packet exists to prevent: a keyless provider whose
    // rotation was refused must stay keyless, in memory and on disk.
    seedStoredProvider(false);
    keyIpcFails = true;

    await expect(
      addProvider({
        id: "srv-9",
        name: "Cloudy",
        baseUrl: "https://api.example.com/v1",
        apiKey: "sk-refused-secret-value",
        kind: "cloud",
        supportsNativeTools: true,
      }),
    ).rejects.toThrow("keychain write refused");

    expect(providersStore.providers[0].hasApiKey).toBe(false);
    const persisted = JSON.parse(localStorage.getItem("lh.providers.v1")!);
    expect(persisted[0].hasApiKey).toBe(false);
    expect(dumpLocalStorage()).not.toContain("sk-refused-secret-value");
  });

  it("rotate: a rejected key IPC leaves an existing key's state untouched", async () => {
    // Mirror image of the above — a failed rotation must not report the
    // provider as having LOST its (still valid) old key either.
    seedStoredProvider(true);
    keyIpcFails = true;

    await expect(
      addProvider({
        id: "srv-9",
        name: "Cloudy Renamed",
        baseUrl: "https://api.example.com/v1",
        apiKey: "sk-refused-secret-value",
        kind: "cloud",
        supportsNativeTools: true,
      }),
    ).rejects.toThrow("keychain write refused");

    expect(providersStore.providers[0].hasApiKey).toBe(true);
    // The metadata edit that DID succeed is still reflected.
    expect(providersStore.providers[0].name).toBe("Cloudy Renamed");
  });

  it("rotate: a blank key leaves the stored credential alone (no key IPC)", async () => {
    seedStoredProvider(true);

    const updated = await addProvider({
      id: "srv-9",
      name: "Cloudy",
      baseUrl: "https://api.example.com/v1",
      apiKey: "",
      kind: "cloud",
      supportsNativeTools: true,
    });

    expect(keyCalls).toEqual([]);
    expect(invokeMock).not.toHaveBeenCalledWith(
      "set_provider_api_key",
      expect.anything(),
    );
    expect(updated.hasApiKey).toBe(true);
  });

  it("add: a rejected creation IPC never reaches the key IPC", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "add_provider") throw new Error("db insert failed");
      throw new Error(`unexpected IPC command: ${cmd}`);
    });

    await expect(
      addProvider({
        name: "OpenAI",
        baseUrl: "https://api.openai.com/v1",
        apiKey: "sk-never-sent-secret",
        kind: "cloud",
        supportsNativeTools: true,
      }),
    ).rejects.toThrow("db insert failed");

    expect(keyCalls).toEqual([]);
    expect(providersStore.providers).toEqual([]);
    expect(dumpLocalStorage()).not.toContain("sk-never-sent-secret");
  });
});