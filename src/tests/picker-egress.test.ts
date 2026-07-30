/// <reference types="vitest" />

/**
 * What the model picker sends over the wire, per interaction.
 *
 * `list_models` is not a free local read: the backend turns it into a live
 * `GET {base_url}/models` with the endpoint's stored `Authorization: Bearer`
 * key (src-tauri/src/models/client.rs). Every one of those is an authenticated
 * request from this machine to that host — including hosts outside the trust
 * boundary, since a configured cloud provider is one of the rows.
 *
 * The picker briefly fanned that out to EVERY configured provider on EVERY
 * click of the picker button: `onclick={() => { onopen?.(); open = !open; }}`
 * fired on the closing edge too, and the host wired `onopen` to a
 * cache-bypassing re-list. Opening and closing the popover once was two full
 * fan-outs the user never asked for.
 *
 * These tests count the `list_models` IPC calls, which is exactly the count of
 * outbound listing requests, and pin them per interaction.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, fireEvent, screen, waitFor } from "@testing-library/svelte";
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

/** Three providers, two of them public endpoints — so a fan-out is visibly a
 *  fan-out across the trust boundary, not just chatter on the LAN. */
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
  {
    id: "openrouter",
    name: "OpenRouter",
    base_url: "https://openrouter.ai/api/v1",
    kind: "cloud",
    is_private: false,
    trusted_by_name: false,
    supports_native_tools: true,
  },
];

const invokeMock = vi.mocked(invoke);
/** Every provider id a `list_models` went out for, in order. */
let listed: string[] = [];

beforeEach(async () => {
  listed = [];
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
      case "list_models":
        listed.push(args!.provider_id as string);
        return ["model-a"];
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

const picker = () => screen.getByRole("button", { name: /thinking strength/i });

/** Let any pending listing round land before counting. */
async function quiesce() {
  await new Promise((r) => setTimeout(r, 20));
}

describe("model picker — endpoint egress per interaction", () => {
  it("lists each provider once on mount, and not again", async () => {
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(3));
    await waitFor(() => expect(listed).toHaveLength(3));
    await quiesce();

    // The cache absorbs any re-run of the provider-list effect.
    expect([...listed].sort()).toEqual(["my-box", "openai", "openrouter"]);
  });

  it("opening and closing the popover contacts NO endpoint", async () => {
    render(MainScreen);
    await waitFor(() => expect(listed).toHaveLength(3));
    const afterMount = listed.length;

    await fireEvent.click(picker()); // open
    await quiesce();
    await fireEvent.click(picker()); // close — used to fan out a second time
    await quiesce();
    await fireEvent.click(picker()); // open again
    await quiesce();

    expect(listed).toHaveLength(afterMount);
  });

  it("still opens — the fix removed the egress, not the popover", async () => {
    render(MainScreen);
    await waitFor(() => expect(listed).toHaveLength(3));

    await fireEvent.click(picker());
    expect(picker().getAttribute("aria-expanded")).toBe("true");
    expect(await screen.findAllByRole("option")).toHaveLength(3);
  });

  it("re-lists only when the user explicitly asks, once per click", async () => {
    // The genuine goal the old code was reaching for — a model added on a live
    // endpoint must not need an app restart — is preserved, as a labelled
    // action whose cost the user can see.
    render(MainScreen);
    await waitFor(() => expect(listed).toHaveLength(3));
    listed = [];

    await fireEvent.click(picker());
    await fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(listed).toHaveLength(3));
    await quiesce();

    expect([...listed].sort()).toEqual(["my-box", "openai", "openrouter"]);
  });

  it("a refresh goes past the cache — a newly-added model shows up", async () => {
    render(MainScreen);
    await waitFor(() => expect(listed).toHaveLength(3));
    await fireEvent.click(picker());
    expect(await screen.findAllByRole("option")).toHaveLength(3);

    // The endpoint gains a model while the app is running.
    invokeMock.mockImplementation(async (cmd: string, payload?: unknown) => {
      const args = (payload as { args?: Record<string, any> } | undefined)?.args;
      if (cmd === "list_providers") return PROVIDERS;
      if (cmd === "list_models") {
        listed.push(args!.provider_id as string);
        return args!.provider_id === "my-box"
          ? ["model-a", "model-b"]
          : ["model-a"];
      }
      if (cmd === "list_conversations" || cmd === "get_messages") return [];
      if (cmd === "get_app_version") return "0.1.0-test";
      if (cmd === "get_active_profile") return "personal";
      return null;
    });

    await fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(async () =>
      expect(await screen.findAllByRole("option")).toHaveLength(4),
    );
  });
});
