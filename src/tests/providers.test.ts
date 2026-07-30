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
    expect(providersStore.activeProviderId).toBe(p.id);
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

  it("automatically sets active on first provider", async () => {
    expect(providersStore.activeProviderId).toBeNull();
    const p = await addProvider({
      name: "Test",
      baseUrl: "http://localhost:8080",
      apiKey: "",
      kind: "local",
      supportsNativeTools: false,
    });
    expect(providersStore.activeProviderId).toBe(p.id);
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

  it("switches active when the active provider is removed", async () => {
    const ids = providersStore.providers.map((p) => p.id);
    providersStore.activeProviderId = ids[0];

    await removeProvider(ids[0]);
    // Active should fall back to the remaining provider (or null)
    expect(providersStore.activeProviderId).toBe(ids[1]);
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
    providersStore.activeProviderId = "test-id";
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

  it("sets the active provider and model", () => {
    const id = providersStore.providers[0].id;
    setActiveModel(id, "gpt-4");

    expect(providersStore.activeProviderId).toBe(id);
    expect(providersStore.activeModel).toBe("gpt-4");
  });

  it("is a no-op for an unknown provider id", () => {
    providersStore.activeProviderId = providersStore.providers[0].id;
    providersStore.activeModel = "original";

    setActiveModel("unknown-id", "gpt-4");

    expect(providersStore.activeProviderId).toBe(providersStore.providers[0].id);
    expect(providersStore.activeModel).toBe("original");
  });

  it("persists the active selection to localStorage", () => {
    const id = providersStore.providers[0].id;
    setActiveModel(id, "claude-3");

    const raw = localStorage.getItem("lh.providers.active.v1");
    expect(raw).toBeTruthy();
    const parsed = JSON.parse(raw!);
    expect(parsed.providerId).toBe(id);
    expect(parsed.model).toBe("claude-3");
  });
});

describe("providers store — resetProviders", () => {
  it("clears everything including localStorage and modelCache", async () => {
    await addProvider({ name: "P1", baseUrl: "http://localhost:1", apiKey: "", kind: "local", supportsNativeTools: false });
    providersStore.activeModel = "some-model";

    resetProviders();

    expect(providersStore.providers).toEqual([]);
    expect(providersStore.activeProviderId).toBeNull();
    expect(providersStore.activeModel).toBeNull();
    // localStorage should be cleared (persisted as empty array / null entry)
    expect(localStorage.getItem("lh.providers.v1")).toBe("[]");
    expect(localStorage.getItem("lh.providers.active.v1")).toBe(
      JSON.stringify({ providerId: null, model: null }),
    );
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
    expect(first).toEqual(["default"]);
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

  it("selects first provider as active when none was selected", async () => {
    seedLocalStorage([
      { id: "a1", name: "Alpha", base_url: "http://localhost:1", kind: "local", is_private: true, trusted_by_name: false, supports_native_tools: false },
    ]);
    providersStore.activeProviderId = null;

    await hydrateProviders();

    expect(providersStore.activeProviderId).toBeTruthy();
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
    providersStore.activeProviderId = "srv-9";
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