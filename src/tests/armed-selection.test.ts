/// <reference types="vitest" />

/**
 * The composer's armed selection is ONE fact, read the same way everywhere.
 *
 * The endpoint-routing fix's whole point is that the UI must not say one thing
 * while the send does another. The picker chip broke that again in a new place:
 * it found the "selected" model by searching the FETCHED model listings for the
 * active key, so a provider whose `GET /models` failed (`items: []`) produced no
 * match and the chip fell through to the amber "No model selected" placeholder
 * — while `providersStore.active` was still set, `canSend` was still true, and
 * clicking Send still sent to that provider.
 *
 * A failed listing is a fact about our knowledge of the endpoint. It is not a
 * fact about what the user selected, and it must never be able to make an armed
 * selection look unarmed.
 *
 * These tests drive the real `MainScreen` composer and pin the chip's text, the
 * Send button's armed state, and the provider that actually reaches the
 * `send_message` IPC to the same source of truth.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, fireEvent, screen, waitFor } from "@testing-library/svelte";
import MainScreen from "$lib/design/screens/MainScreen.svelte";
import {
  providersStore,
  resetProviders,
  hydrateProviders,
  setActiveModel,
} from "$lib/stores/providers.svelte";
import { conversations, activeConversationId } from "$lib/stores/chat";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));
import { invoke } from "@tauri-apps/api/core";

// ── Fixtures ────────────────────────────────────────────────────────────────

const PROVIDERS = [
  {
    id: "my-box",
    name: "My box",
    base_url: "http://10.0.0.5:8000/v1",
    kind: "custom",
    is_private: true,
    trusted_by_name: false,
    supports_native_tools: true,
  },
  {
    id: "openai",
    name: "OpenAI",
    base_url: "https://api.openai.com/v1",
    kind: "cloud",
    is_private: false,
    trusted_by_name: false,
    supports_native_tools: true,
  },
  // The row an EXISTING install still has in global.db. Removing the quick-add
  // preset only helps new installs, and we deliberately don't migrate or delete
  // the user's data — so this provider is configured, answers, lists nothing,
  // and can never be selected. The picker owes the user that explanation.
  {
    id: "anthropic",
    name: "Anthropic",
    base_url: "https://api.anthropic.com/v1",
    kind: "cloud",
    is_private: false,
    trusted_by_name: false,
    supports_native_tools: true,
  },
];

const MODELS_BY_PROVIDER: Record<string, string[]> = {
  "my-box": ["qwen3-32b"],
  openai: ["gpt-4o"],
  // Answers fine; lists nothing. A Bearer key against Anthropic's native API
  // never yields an OpenAI-shaped model list.
  anthropic: [],
};

/** Providers whose `list_models` currently refuses — an endpoint that is down,
 *  behind a rotated key, or simply not OpenAI-compatible. */
let listingFails = new Set<string>();

type SendArgs = { provider_id: string; model: string; content: string };

const invokeMock = vi.mocked(invoke);
let sendCalls: SendArgs[] = [];

beforeEach(async () => {
  sendCalls = [];
  listingFails = new Set();
  conversations.set([]);
  activeConversationId.set(null);
  resetProviders();
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, payload?: unknown) => {
    const args = (payload as { args?: Record<string, any> } | undefined)?.args;
    switch (cmd) {
      case "list_providers":
        return PROVIDERS;
      case "list_models": {
        const id = args!.provider_id as string;
        if (listingFails.has(id)) throw new Error("connection refused");
        return MODELS_BY_PROVIDER[id] ?? [];
      }
      case "list_conversations":
        return [];
      case "get_messages":
        return [];
      case "create_conversation":
        return {
          id: "conv-1",
          name: args!.name,
          pinned: false,
          binding: args!.binding,
          folder_id: null,
          color: null,
          created_at: 0,
          updated_at: 0,
        };
      case "send_message":
        sendCalls.push(args as SendArgs);
        return {
          message_id: `msg-${sendCalls.length}`,
          content: "ok",
          conversation_id: args!.conversation_id,
          profile: args!.profile,
          routing_decision: "allow",
          served_by: {
            provider_id: args!.provider_id,
            provider_name:
              PROVIDERS.find((p) => p.id === args!.provider_id)?.name ?? null,
            base_url:
              PROVIDERS.find((p) => p.id === args!.provider_id)?.base_url ?? null,
            zone: PROVIDERS.find((p) => p.id === args!.provider_id)?.is_private
              ? "local"
              : "cloud",
          },
          completed_at: 0,
        };
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

// ── Helpers ─────────────────────────────────────────────────────────────────

/** The composer's model chip. Its accessible name carries the visible
 *  provider·model label plus the sr-only thinking strength. */
const chip = () => screen.getByRole("button", { name: /thinking strength/i });

/** The armed Send button, if the composer thinks it can send. Its accessible
 *  name is "Send via …" only when `canSend` is true; unarmed it reads
 *  "Can't send — pick a model first" and is disabled. */
const armedSend = () => screen.queryByRole("button", { name: /^Send via/ });

async function settle() {
  await waitFor(() => expect(providersStore.providers).toHaveLength(3));
  await waitFor(() =>
    expect(chip().textContent).toMatch(/No model selected|·/),
  );
}

// ── Tests ───────────────────────────────────────────────────────────────────

describe("composer chip — armed selection vs. failed model listing", () => {
  it("still shows the armed endpoint when its model listing failed", async () => {
    // The endpoint the user armed has gone unreachable. The selection is
    // untouched — the app will still send there — so the chip must still say so.
    listingFails.add("my-box");
    render(MainScreen);
    await settle();

    expect(setActiveModel("my-box", "qwen3-32b")).toBe(true);

    await waitFor(() => expect(chip().textContent).toContain("qwen3-32b"));
    expect(chip().textContent).toContain("My box");
    // The exact regression: the chip claimed nothing was selected.
    expect(chip().textContent).not.toContain("No model selected");
  });

  it("keeps Send armed, and the send goes to the endpoint the chip names", async () => {
    listingFails.add("my-box");
    render(MainScreen);
    await settle();
    expect(setActiveModel("my-box", "qwen3-32b")).toBe(true);
    await waitFor(() => expect(chip().textContent).toContain("qwen3-32b"));

    // Send is armed — which was ALREADY true before this fix. The bug was that
    // the chip disagreed with it.
    const send = armedSend();
    expect(send).not.toBeNull();
    expect(send).not.toBeDisabled();

    const textarea = screen.getByPlaceholderText("Message Lost Harness…");
    await fireEvent.input(textarea, { target: { value: "hello" } });
    await fireEvent.click(send!);

    await waitFor(() => expect(sendCalls).toHaveLength(1));
    expect(sendCalls[0]).toMatchObject({
      provider_id: "my-box",
      model: "qwen3-32b",
    });
  });

  it("marks it UNCONFIRMED — a distinct state from unarmed", async () => {
    // "Armed, but we couldn't list this endpoint's models" is its own thing.
    // Collapsing it into "nothing selected" is the lie; collapsing it into a
    // healthy chip would hide a real problem. It gets its own affordance.
    listingFails.add("my-box");
    render(MainScreen);
    await settle();
    expect(setActiveModel("my-box", "qwen3-32b")).toBe(true);

    await waitFor(() => expect(chip().textContent).toContain("qwen3-32b"));
    expect(chip().textContent).toContain("Couldn't list models");
    expect(chip().getAttribute("title")).toContain(
      "Sending to My box · qwen3-32b",
    );
  });

  it("does NOT flag a healthy endpoint as unconfirmed", async () => {
    render(MainScreen);
    await settle();
    expect(setActiveModel("openai", "gpt-4o")).toBe(true);

    await waitFor(() => expect(chip().textContent).toContain("gpt-4o"));
    expect(chip().textContent).not.toContain("Couldn't list models");
    expect(chip().getAttribute("title")).toBe(
      "Sending to OpenAI · gpt-4o — click to change",
    );
  });
});

describe("composer chip — one source of truth with canSend", () => {
  it("shows the placeholder EXACTLY when Send is unarmed", async () => {
    render(MainScreen);
    await settle();

    // Nothing armed: placeholder AND no "Send via" button.
    expect(providersStore.active).toBeNull();
    expect(chip().textContent).toContain("No model selected");
    expect(armedSend()).toBeNull();
    expect(
      screen.getByRole("button", { name: /pick a model first/i }),
    ).toBeDisabled();

    // Armed on a DEAD endpoint: no placeholder AND a live "Send via" button.
    // These two must flip together; the bug was that only one of them did.
    listingFails.add("my-box");
    expect(setActiveModel("my-box", "qwen3-32b")).toBe(true);
    await waitFor(() => expect(armedSend()).not.toBeNull());
    expect(chip().textContent).not.toContain("No model selected");
  });

  it("an endpoint that vanishes disarms BOTH the chip and Send", async () => {
    // The one case where an armed pair legitimately stops counting: the
    // provider is no longer configured. Then the chip and Send must BOTH go
    // back to unarmed — the composer really cannot send anywhere.
    render(MainScreen);
    await settle();
    expect(setActiveModel("openai", "gpt-4o")).toBe(true);
    await waitFor(() => expect(armedSend()).not.toBeNull());

    providersStore.providers = providersStore.providers.filter(
      (p) => p.id !== "openai",
    );

    await waitFor(() => expect(armedSend()).toBeNull());
    expect(chip().textContent).toContain("No model selected");
  });
});

describe("composer chip — a listing that fails AFTER the user picked", () => {
  it("survives a refresh that turns the armed endpoint's listing into an error", async () => {
    render(MainScreen);
    await settle();

    // Pick through the real popover while the endpoint is healthy.
    await fireEvent.click(chip());
    const option = await screen.findByRole("option", { name: /qwen3-32b/ });
    await fireEvent.click(option);
    await waitFor(() => expect(chip().textContent).toContain("qwen3-32b"));

    // The endpoint goes down; the user hits Refresh and the listing now errors.
    listingFails.add("my-box");
    await fireEvent.click(chip());
    await fireEvent.click(screen.getByRole("button", { name: "Refresh" }));

    await waitFor(() =>
      expect(chip().textContent).toContain("Couldn't list models"),
    );
    // Still armed, still named, still sendable — nothing about the SELECTION
    // changed just because we could no longer enumerate the endpoint.
    expect(chip().textContent).toContain("My box");
    expect(chip().textContent).toContain("qwen3-32b");
    expect(providersStore.active).toEqual({
      providerId: "my-box",
      model: "qwen3-32b",
    });
    expect(armedSend()).not.toBeNull();
  });
});

describe("picker notice — an endpoint that lists no models", () => {
  it("explains that this app can't use it, not just that it is empty", async () => {
    // "This endpoint listed no models." was true and useless: it reads like a
    // transient blank, so the user waits for models that will never arrive. The
    // notice has to name the actual dead end.
    render(MainScreen);
    await settle();
    await fireEvent.click(chip());

    const popover = await screen.findByRole("listbox");
    await waitFor(() =>
      expect(popover.textContent).toContain("listed no models"),
    );
    // What the user needs in order to act: the protocol requirement, and the
    // two things they can do about it.
    expect(popover.textContent).toContain("OpenAI-compatible");
    expect(popover.textContent).toContain("/chat/completions");
    expect(popover.textContent).toContain("/v1");
    expect(popover.textContent).toMatch(/remove it/i);
  });

  it("keeps the dead endpoint LISTED rather than hiding it", async () => {
    // Dropping an empty provider from the popover is what left a different
    // endpoint armed and serving every turn, invisibly. It stays, with its
    // reason.
    render(MainScreen);
    await settle();
    await fireEvent.click(chip());

    const popover = await screen.findByRole("listbox");
    expect(popover.textContent).toContain("Anthropic");
  });
});
