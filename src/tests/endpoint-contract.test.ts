/// <reference types="vitest" />

/**
 * The picker→request contract (2026-07-29 endpoint-routing spec, Item 1).
 *
 * The governing invariant: **a turn goes to exactly the provider the user
 * selected, or it fails loudly — never a silent substitution.** A wrong
 * provider is a wrong TRUST ZONE, not a cosmetic mix-up, so these tests are
 * privacy tests.
 *
 * They drive the REAL composer (`MainScreen.svelte`) inside a stubbed Tauri
 * runtime and assert on the arguments that reach the `send_message` IPC —
 * the last frontend boundary before the request leaves for an endpoint. The
 * whole path is exercised: model picker → owner map → provider store →
 * chat store → invoke. Any layer that substituted a provider or model would
 * show up here as a mismatch between what was clicked and what was sent.
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

// ── The fake installed app ──────────────────────────────────────────────────

/** Two providers whose model lists deliberately COLLIDE on a name. Keying
 *  ownership by bare model name is the 6dfcf12 bug; if it were reintroduced,
 *  one of these would silently shadow the other. */
const PROVIDERS = [
  {
    id: "anthropic",
    name: "Anthropic",
    base_url: "https://api.anthropic.com/v1",
    kind: "cloud",
    is_private: false,
    trusted_by_name: false,
    supports_native_tools: true,
  },
  {
    id: "my-openai",
    name: "My OpenAI box",
    base_url: "http://10.0.0.5:8000/v1",
    kind: "custom",
    is_private: true,
    trusted_by_name: false,
    supports_native_tools: true,
  },
];

const MODELS_BY_PROVIDER: Record<string, string[]> = {
  // "shared-model" exists on BOTH endpoints — same string, different vendors,
  // different trust zones.
  anthropic: ["shared-model", "claude-sonnet-4"],
  "my-openai": ["shared-model", "qwen3-32b"],
};

type SendArgs = { provider_id: string; model: string; content: string };

const invokeMock = vi.mocked(invoke);
let sendCalls: SendArgs[] = [];

beforeEach(async () => {
  sendCalls = [];
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
        return MODELS_BY_PROVIDER[args!.provider_id] ?? [];
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
          // Unique per turn, like the real backend — a repeated id would
          // collide in the transcript's keyed each block.
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
            // The backend stamps the turn's trust zone from the endpoint's
            // own privacy (`!is_private`), never from its `kind` label — both
            // fixtures below are private addresses, so both are "local".
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

/** Open the model popover and click the model listed under `providerName`. */
async function pickModel(providerName: string, modelName: string) {
  const openPicker = await screen.findByRole("button", {
    name: /thinking strength/i,
  });
  await fireEvent.click(openPicker);

  // Options are grouped by provider; find the one in the right group by
  // walking the group container, so a same-named model on the other endpoint
  // cannot be clicked by accident.
  const options = await screen.findAllByRole("option");
  const match = options.find((el) => {
    const groupEl = el.parentElement;
    return (
      el.textContent?.includes(modelName) &&
      groupEl?.textContent?.includes(providerName)
    );
  });
  if (!match) {
    throw new Error(
      `no "${modelName}" option under "${providerName}"; saw: ${options
        .map((o) => o.textContent?.trim())
        .join(" | ")}`,
    );
  }
  await fireEvent.click(match);
}

async function typeAndSend(text: string) {
  const textarea = screen.getByPlaceholderText("Message Lost Harness…");
  await fireEvent.input(textarea, { target: { value: text } });
  const send = screen.getByRole("button", { name: /^Send via/ });
  await fireEvent.click(send);
}

// ── Tests ───────────────────────────────────────────────────────────────────

describe("picker → request contract", () => {
  it("sends to EXACTLY the provider and model the user picked", async () => {
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    await pickModel("My OpenAI box", "qwen3-32b");
    await typeAndSend("hello");

    await waitFor(() => expect(sendCalls).toHaveLength(1));
    expect(sendCalls[0].provider_id).toBe("my-openai");
    expect(sendCalls[0].model).toBe("qwen3-32b");
    expect(sendCalls[0].content).toBe("hello");
  });

  it("does not substitute the alphabetically-first provider", async () => {
    // The reported symptom: a turn aimed at an OpenAI endpoint arriving at
    // Anthropic's. "Anthropic" sorts first, and the backend lists providers
    // `ORDER BY name`, so every "just use the first one" path lands there.
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    await pickModel("My OpenAI box", "qwen3-32b");
    await typeAndSend("hello");

    await waitFor(() => expect(sendCalls).toHaveLength(1));
    expect(sendCalls[0].provider_id).not.toBe("anthropic");
  });

  it("keeps identically-named models on different endpoints distinct", async () => {
    // Both providers expose "shared-model". Ownership is keyed by
    // `providerId::name`, so picking it under one provider can never address
    // the other — the name-collision bug fixed in 6dfcf12, pinned here.
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    await pickModel("My OpenAI box", "shared-model");
    await typeAndSend("first");

    await waitFor(() => expect(sendCalls).toHaveLength(1));
    expect(sendCalls[0]).toMatchObject({
      provider_id: "my-openai",
      model: "shared-model",
    });

    await pickModel("Anthropic", "shared-model");
    await typeAndSend("second");

    await waitFor(() => expect(sendCalls).toHaveLength(2));
    expect(sendCalls[1]).toMatchObject({
      provider_id: "anthropic",
      model: "shared-model",
    });
  });

  it("refuses to send at all when nothing is selected", async () => {
    // Fail closed. The composer used to send `provider_id: ""` / `model: ""`
    // and let the backend reject it; now no request is made and the user is
    // told why.
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    // Nothing picked: hydration must not have armed anything.
    expect(providersStore.active).toBeNull();

    const textarea = screen.getByPlaceholderText("Message Lost Harness…");
    await fireEvent.input(textarea, { target: { value: "leak me" } });

    const send = screen.getByRole("button", { name: /pick a model first/i });
    expect(send).toBeDisabled();
    await fireEvent.click(send);

    expect(sendCalls).toEqual([]);
    // The draft is preserved — refusing must not eat the user's text.
    expect((textarea as HTMLTextAreaElement).value).toBe("leak me");
  });

  it("shows the selected provider AND model on the composer, visibly", async () => {
    // A trust-zone decision must be readable while typing, not only through a
    // screen reader (the 2026-07-25 rework had left it sr-only).
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    await pickModel("My OpenAI box", "qwen3-32b");

    const label = await screen.findByTitle(/Sending to My OpenAI box · qwen3-32b/);
    expect(label).toBeInTheDocument();
    expect(label.textContent).toContain("My OpenAI box");
    expect(label.textContent).toContain("qwen3-32b");
    // Not hidden from sight.
    expect(label.querySelector(".sr-only")?.textContent).not.toBe(
      label.textContent,
    );
  });

  it("keeps a provider whose model listing failed visible in the picker", async () => {
    // A provider that silently disappears from the popover is what leaves a
    // DIFFERENT provider armed and serving every turn. It stays listed, with
    // the reason.
    invokeMock.mockImplementation(async (cmd: string, payload?: unknown) => {
      const args = (payload as { args?: Record<string, any> } | undefined)?.args;
      if (cmd === "list_providers") return PROVIDERS;
      if (cmd === "list_models") {
        if (args!.provider_id === "my-openai") {
          throw new Error("connection refused");
        }
        return MODELS_BY_PROVIDER[args!.provider_id] ?? [];
      }
      if (cmd === "get_app_version") return "0.1.0-test";
      return null;
    });

    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    const openPicker = await screen.findByRole("button", {
      name: /thinking strength/i,
    });
    await fireEvent.click(openPicker);

    // Still listed...
    expect(await screen.findByText("My OpenAI box")).toBeInTheDocument();
    // ...with an actionable explanation rather than a blank gap.
    const notice = await screen.findByText(/couldn't list models/i);
    expect(notice.textContent).toContain("connection refused");
  });
});

describe("per-turn route indicator", () => {
  /** Make `send_message` answer as though the privacy gate rerouted the turn
   *  to a local model instead of the cloud endpoint the composer picked. */
  function rerouteToLocal() {
    const base = invokeMock.getMockImplementation()!;
    invokeMock.mockImplementation(async (cmd: string, payload?: any) => {
      if (cmd !== "send_message") return base(cmd, payload);
      const args = (payload as { args: Record<string, any> }).args;
      sendCalls.push(args as SendArgs);
      return {
        message_id: `msg-${sendCalls.length}`,
        content: "answered locally",
        conversation_id: args.conversation_id,
        profile: args.profile,
        routing_decision: "route_local",
        served_by: {
          provider_id: "local-llm",
          provider_name: "Local llama",
          base_url: "http://127.0.0.1:11434/v1",
          zone: "local",
        },
        completed_at: 0,
      };
    });
  }

  it("names the provider AND endpoint that actually served the turn", async () => {
    // docs/TECH-DEBT.md §1: the FINAL authoritative state wins over the
    // pre-send prediction. On a privacy reroute the serving endpoint is a
    // DIFFERENT provider than the picker shows — which is exactly the case
    // where the composer's own label would mislead.
    rerouteToLocal();
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    await pickModel("My OpenAI box", "qwen3-32b");
    await typeAndSend("my SSN is 123-45-6789");

    await waitFor(() => expect(sendCalls).toHaveLength(1));
    // The request itself still went exactly where the user picked...
    expect(sendCalls[0].provider_id).toBe("my-openai");

    // ...and the badge reports where it was ACTUALLY served: provider name
    // plus endpoint host, not just a model string.
    const badge = await screen.findByRole("button", { name: /Local llama/ });
    expect(badge.textContent).toContain("Local llama");
    expect(badge.textContent).toContain("127.0.0.1:11434");
    // The full endpoint is available on hover for the exact answer.
    expect(badge).toHaveAttribute("title", "http://127.0.0.1:11434/v1");
    // The composer's predicted model is NOT shown beside the rerouted
    // provider — it was the model on the other endpoint, and pairing them
    // would be a small, confident lie.
    expect(badge.textContent).not.toContain("qwen3-32b");
  });

  it("labels a normal turn with the endpoint the user picked", async () => {
    render(MainScreen);
    await waitFor(() => expect(providersStore.providers).toHaveLength(2));

    await pickModel("My OpenAI box", "qwen3-32b");
    await typeAndSend("hello");

    await waitFor(() => expect(sendCalls).toHaveLength(1));

    // Anchored so this matches the routing badge, not the composer's own
    // (correctly) identical provider label.
    const badge = await screen.findByRole("button", {
      name: /^Local · My OpenAI box/,
    });
    expect(badge.textContent).toContain("My OpenAI box");
    expect(badge.textContent).toContain("10.0.0.5:8000");
    // No reroute, so the model the user picked is still the truthful one.
    expect(badge.textContent).toContain("qwen3-32b");
  });
});
