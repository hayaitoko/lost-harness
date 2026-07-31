/// <reference types="vitest" />

/**
 * Picker identity: a provider is its ID, never its display name.
 *
 * Provider names are NOT unique. Ids are backend UUIDs (`Uuid::new_v4()` in
 * `ipc/mod.rs::add_provider_inner`), the global `endpoints` table has no
 * `UNIQUE(name)`, and the add-provider form has no duplicate-name guard — so
 * two providers both called "Ollama" (or the same quick-add preset clicked
 * twice) is a state the app can genuinely be in.
 *
 * When the picker's section list was keyed by the display name, that state
 * made Svelte throw `each_key_duplicate`. The popover is ALWAYS mounted (it is
 * hidden with opacity/pointer-events, not `{#if}`), so the throw took down the
 * entire MainScreen render — the user lost the whole composer, not just the
 * model list.
 *
 * These tests pin both halves: the ModelPicker component itself, and the real
 * composer built from a real provider list.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, fireEvent, screen, waitFor } from "@testing-library/svelte";
import ModelPicker, {
  type ModelGroup,
} from "$lib/design/components/ModelPicker.svelte";
import MainScreen from "$lib/design/screens/MainScreen.svelte";
import {
  providersStore,
  resetProviders,
  hydrateProviders,
} from "$lib/stores/providers.svelte";
import { conversations, activeConversationId } from "$lib/stores/chat";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));
import { invoke } from "@tauri-apps/api/core";

// ── ModelPicker in isolation ────────────────────────────────────────────────

describe("ModelPicker — duplicate provider names", () => {
  it("renders two identically-named providers without throwing", () => {
    // Same display name, different ids — exactly what two `Ollama` endpoints
    // look like. Keyed by name this threw `each_key_duplicate`.
    const groups: ModelGroup[] = [
      {
        id: "11111111-1111-1111-1111-111111111111",
        group: "Ollama",
        kind: "local",
        items: [{ name: "llama3", key: "11111111-1111-1111-1111-111111111111::llama3" }],
        notice: null,
      },
      {
        id: "22222222-2222-2222-2222-222222222222",
        group: "Ollama",
        kind: "local",
        items: [{ name: "qwen3", key: "22222222-2222-2222-2222-222222222222::qwen3" }],
        notice: null,
      },
    ];

    const { container } = render(ModelPicker, { groups, selection: null });

    // Both sections rendered — neither provider was swallowed.
    const options = container.querySelectorAll('[role="option"]');
    expect(Array.from(options).map((o) => o.textContent?.trim())).toEqual([
      "llama3",
      "qwen3",
    ]);
  });

  it("still distinguishes the two when one of them is selected", () => {
    const groups: ModelGroup[] = [
      {
        id: "prov-a",
        group: "Ollama",
        kind: "local",
        items: [{ name: "llama3", key: "prov-a::llama3" }],
        notice: null,
      },
      {
        id: "prov-b",
        group: "Ollama",
        kind: "local",
        items: [{ name: "llama3", key: "prov-b::llama3" }],
        notice: null,
      },
    ];

    // Same NAME on both endpoints too — the composite key is what keeps the
    // checkmark on the right one.
    const { container } = render(ModelPicker, {
      groups,
      selection: {
        key: "prov-b::llama3",
        model: "llama3",
        provider: "Ollama",
        kind: "local" as const,
      },
    });

    const options = Array.from(container.querySelectorAll('[role="option"]'));
    expect(options).toHaveLength(2);
    expect(options[0].getAttribute("aria-selected")).toBe("false");
    expect(options[1].getAttribute("aria-selected")).toBe("true");
  });

  it("does not drop an empty group that shares a name with a full one", () => {
    // The failed-listing group carries a notice and no items; it must still
    // appear beside its same-named twin.
    const groups: ModelGroup[] = [
      {
        id: "prov-a",
        group: "Ollama",
        kind: "local",
        items: [{ name: "llama3", key: "prov-a::llama3" }],
        notice: null,
      },
      {
        id: "prov-b",
        group: "Ollama",
        kind: "local",
        items: [],
        notice: "Couldn't list models — check the endpoint or key.",
      },
    ];

    const { container } = render(ModelPicker, { groups, selection: null });

    expect(container.textContent).toContain(
      "Couldn't list models — check the endpoint or key.",
    );
    expect(container.querySelectorAll('[role="option"]')).toHaveLength(1);
  });
});

// ── The real composer ───────────────────────────────────────────────────────

/** Two providers the user added under the same name, plus a third that
 *  returns a duplicated model name — both are `each` key collisions. */
const DUPE_PROVIDERS = [
  {
    id: "aaaa-1111",
    name: "Ollama",
    base_url: "http://127.0.0.1:11434/v1",
    kind: "local",
    is_private: true,
    trusted_by_name: false,
    supports_native_tools: false,
  },
  {
    id: "bbbb-2222",
    name: "Ollama",
    base_url: "http://10.0.0.26:11434/v1",
    kind: "custom",
    is_private: true,
    trusted_by_name: false,
    supports_native_tools: false,
  },
];

const DUPE_MODELS: Record<string, string[]> = {
  "aaaa-1111": ["llama3", "llama3", "qwen3"], // endpoint repeats a model name
  "bbbb-2222": ["llama3"],
};

const invokeMock = vi.mocked(invoke);

beforeEach(async () => {
  conversations.set([]);
  activeConversationId.set(null);
  resetProviders();
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, payload?: unknown) => {
    const args = (payload as { args?: Record<string, any> } | undefined)?.args;
    switch (cmd) {
      case "list_providers":
        return DUPE_PROVIDERS;
      case "list_models":
        return DUPE_MODELS[args!.provider_id] ?? [];
      case "list_conversations":
        return [];
      case "get_messages":
        return [];
      case "get_app_version":
        return "0.1.0-test";
      case "get_active_profile":
        return "personal";
      default:
        return null;
    }
  });
  await hydrateProviders();
});

afterEach(() => {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined;
  invokeMock.mockReset();
});

describe("composer — duplicate provider names", () => {
  it("renders the whole composer with two same-named providers configured", async () => {
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    // The composer survived: its textarea and the picker button are both here.
    // A thrown `each_key_duplicate` in the always-mounted popover would have
    // taken this entire screen with it.
    await waitFor(() =>
      expect(
        screen.getByPlaceholderText("Message Lost Harness…"),
      ).toBeInTheDocument(),
    );
    const openPicker = await screen.findByRole("button", {
      name: /thinking strength/i,
    });
    await fireEvent.click(openPicker);

    // Both endpoints are listed and addressable, with the repeated model name
    // from the first endpoint de-duplicated to a single option.
    const options = await screen.findAllByRole("option");
    expect(options.map((o) => o.textContent?.trim())).toEqual([
      "llama3",
      "qwen3",
      "llama3",
    ]);
  });
});
