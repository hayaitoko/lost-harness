/// <reference types="vitest" />

/**
 * One endpoint, one model name, one list entry — in EVERY consumer.
 *
 * Nothing on the OpenAI-compatible wire forbids `GET /models` from listing the
 * same id twice, and real deployments do it: a gateway that fans one route out
 * to several upstreams (LiteLLM, two llama.cpp servers behind one proxy)
 * returns the id once per upstream. Every list the app builds from that
 * response is KEYED by model name — the composer's popover
 * (`providerId::name`) and the Settings → Models `<Select>` (`item.value`) —
 * and Svelte throws `each_key_duplicate` on a repeated key, which does not
 * degrade gracefully: it blanks the screen.
 *
 * The first fix for this deduped inside `MainScreen.loadModelGroups`, i.e. at
 * ONE call site. `Settings.svelte` reads the same store, stores `result.models`
 * verbatim, and feeds it straight to a `<Select>` — so it still crashed. These
 * tests therefore pin the guarantee at the source (`fetchModels`) AND drive
 * BOTH real screens, because a per-call-site dedup is exactly the shape of fix
 * that leaves the next consumer broken.
 *
 * The backend dedupes too (`models::client::distinct_model_ids`, pinned in
 * `src-tauri/src/models/tests.rs`); the store-level guarantee is what covers
 * the browser-fallback bridge and any endpoint whose listing did not come
 * through our client.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, fireEvent, screen, waitFor, cleanup } from "@testing-library/svelte";
import { within } from "@testing-library/dom";
import MainScreen from "$lib/design/screens/MainScreen.svelte";
import Settings from "$lib/design/screens/Settings.svelte";
import {
  providersStore,
  resetProviders,
  hydrateProviders,
  fetchModels,
} from "$lib/stores/providers.svelte";
import { conversations, activeConversationId } from "$lib/stores/chat";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));
import { invoke } from "@tauri-apps/api/core";

/** One endpoint. Its `GET /models` answers with the same two names repeated —
 *  the shape a fan-out proxy really returns. */
const PROVIDERS = [
  {
    id: "gateway",
    name: "My gateway",
    base_url: "http://10.0.0.5:4000/v1",
    kind: "custom",
    is_private: true,
    trusted_by_name: false,
    supports_native_tools: true,
  },
];

const DUPLICATED = ["qwen3-32b", "gpt-4o", "qwen3-32b", "gpt-4o", "qwen3-32b"];

const invokeMock = vi.mocked(invoke);

beforeEach(async () => {
  conversations.set([]);
  activeConversationId.set(null);
  resetProviders();
  localStorage.clear();
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "list_providers":
        return PROVIDERS;
      case "list_models":
        return [...DUPLICATED];
      case "list_conversations":
        return [];
      case "get_messages":
        return [];
      case "get_app_version":
        return "0.1.0-test";
      case "get_active_profile":
        return "personal";
      default:
        // Settings' Models pane also loads downloaded local models and model
        // seats on mount. Answer every other `list_*` with an empty array (not
        // the `null` a bare default would give) so an unrelated pane can't
        // throw and be mistaken for the keyed-each crash under test.
        return cmd.startsWith("list_") ? [] : null;
    }
  });
  await hydrateProviders();
});

afterEach(() => {
  cleanup();
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined;
  invokeMock.mockReset();
});

describe("fetchModels — the store is where the guarantee lives", () => {
  it("returns each model name once, in the order the endpoint first listed it", async () => {
    // Fixing this in the store (and the backend) rather than in a component is
    // the whole point: the NEXT consumer gets it for free.
    const result = await fetchModels("gateway");
    expect(result).toEqual({ ok: true, models: ["qwen3-32b", "gpt-4o"] });
  });

  it("serves the deduped list from cache too, not the raw one", async () => {
    await fetchModels("gateway");
    const cached = await fetchModels("gateway");
    expect(cached).toEqual({ ok: true, models: ["qwen3-32b", "gpt-4o"] });

    // …and a cache-bypassing refresh re-dedupes rather than re-introducing it.
    const refreshed = await fetchModels("gateway", { refresh: true });
    expect(refreshed).toEqual({ ok: true, models: ["qwen3-32b", "gpt-4o"] });
  });
});

describe("Settings → Models — the call site the one-site patch missed", () => {
  it("keeps the endpoint's models selectable instead of blanking the row", async () => {
    // THE regression, in the shape the user meets it. `Settings.svelte` stored
    // `result.models` verbatim and handed it to `<Select>`, whose
    // `{#each items as item (item.value)}` throws `each_key_duplicate` on a
    // repeated value. Svelte unwinds that render, and the row ends up showing
    // a DISABLED "No models" dropdown — the endpoint is configured, answers,
    // lists models, and cannot be selected from Settings at all.
    render(Settings);
    await fireEvent.click(screen.getByRole("button", { name: "Models" }));

    await waitFor(() => {
      // The provider row's own description line — unique to the row (the
      // provider NAME also appears in the seat-binding <select> below it).
      expect(
        screen.getByText(/http:\/\/10\.0\.0\.5:4000\/v1/),
      ).toBeInTheDocument();
    });

    const trigger = await screen.findByRole("button", { name: /Select model/ });
    expect(trigger).not.toBeDisabled();
    expect(screen.queryByRole("button", { name: /No models/ })).toBeNull();
  });

  it("offers each model name exactly once in the row's Select", async () => {
    render(Settings);
    await fireEvent.click(screen.getByRole("button", { name: "Models" }));
    await waitFor(() =>
      expect(screen.getByText(/http:\/\/10\.0\.0\.5:4000\/v1/)).toBeInTheDocument(),
    );

    // Open the row's model dropdown, and read options only from THAT Select —
    // the seat-binding <select> further down the pane also publishes options.
    const trigger = await screen.findByRole("button", { name: /Select model/ });
    await fireEvent.click(trigger);

    const options = within(trigger.parentElement!).getAllByRole("option");
    const names = options.map((o) => o.textContent?.trim());
    expect(names).toEqual(["qwen3-32b", "gpt-4o"]);
  });
});

describe("composer picker — the call site that was already patched", () => {
  it("still lists each model name once (now via the store's guarantee)", async () => {
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(1));

    const picker = screen.getByRole("button", { name: /thinking strength/i });
    await fireEvent.click(picker);

    const options = await screen.findAllByRole("option");
    expect(options.map((o) => o.textContent?.trim())).toEqual([
      "qwen3-32b",
      "gpt-4o",
    ]);
  });
});
