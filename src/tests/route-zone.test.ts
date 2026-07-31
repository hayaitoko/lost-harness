/// <reference types="vitest" />

/**
 * The per-turn route badge must never lie about the trust zone.
 *
 * Spec lines 32-38 make wrong-provider a PRIVACY bug: wrong provider = wrong
 * trust zone. The badge used to re-derive that zone frontend-side —
 * `getProvider(...)` against the LIVE provider store, then
 * `provider.kind === "cloud" ? "cloud" : "local"`, with a bare
 * `return "local"` when the provider couldn't be found. So a turn genuinely
 * served by a public cloud endpoint rendered as a reassuring green "Local"
 * badge as soon as that endpoint was deleted. The privacy signal inverted.
 *
 * The fix is structural: the backend stamps the trust zone on the turn when it
 * runs and returns it as `served_by.zone`, and the frontend renders that and
 * only that. These tests drive the REAL transcript (`MainScreen.svelte`) and
 * assert on the badge it puts on screen.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/svelte";
import RoutingBadge from "$lib/design/components/RoutingBadge.svelte";
import MainScreen from "$lib/design/screens/MainScreen.svelte";
import {
  resetProviders,
  hydrateProviders,
} from "$lib/stores/providers.svelte";
import {
  conversations,
  activeConversationId,
  type Conversation,
  type Message,
} from "$lib/stores/chat";
import type { ServedBy } from "$lib/api/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));
import { invoke } from "@tauri-apps/api/core";

// The only provider the app currently knows about: a LOCAL one. Every test
// below serves its turn from somewhere else — so any code path that resolves
// the zone through this store instead of through the stamp would produce a
// green "Local" badge and fail the assertion.
const PROVIDERS = [
  {
    id: "local-llm",
    name: "Local Llama",
    base_url: "http://127.0.0.1:11434/v1",
    kind: "local",
    is_private: true,
    trusted_by_name: false,
    supports_native_tools: false,
  },
];

const invokeMock = vi.mocked(invoke);

beforeEach(async () => {
  conversations.set([]);
  activeConversationId.set(null);
  resetProviders();
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "list_providers":
        return PROVIDERS;
      case "list_models":
        return ["llama3"];
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

/** Put one finished assistant turn on screen, already hydrated, so the
 *  transcript renders its routing badge without any network round-trip. */
function seedTurn(servedBy: ServedBy | null, routingDecision = "allow") {
  const assistant: Message = {
    id: "a1",
    role: "assistant",
    content: "here you go",
    created_at: 1,
    streaming: false,
    routing_decision: routingDecision,
    model: "gpt-x",
    provider_id: servedBy?.provider_id ?? null,
    served_by: servedBy,
  };
  const conv: Conversation = {
    id: "conv-1",
    name: "Chat",
    pinned: false,
    binding: "auto",
    messages: [assistant],
    hydrated: true,
    created_at: 0,
  };
  conversations.set([conv]);
  activeConversationId.set("conv-1");
}

/** The badge's own text, whichever route it landed on. */
async function badgeText(): Promise<string> {
  const badge = await waitFor(() => {
    const el = document.body.querySelector('[class*="rounded-[var(--r-sm)]"]');
    const found = Array.from(
      document.body.querySelectorAll("span, button"),
    ).find((e) =>
      /^(Local|Cloud|Held|Unknown route)\b/.test(e.textContent?.trim() ?? ""),
    );
    if (!found) throw new Error(`no routing badge on screen (${el?.className})`);
    return found;
  });
  return badge.textContent?.trim() ?? "";
}

/** Just the badge's ZONE CLAIM — the first segment of
 *  "<zone> · <provider> (<host>) · <model>". Asserting on the whole label
 *  would trip over a provider literally named "Local Llama"; the claim is
 *  what has to be right. */
async function badgeZone(): Promise<string> {
  return (await badgeText()).split(" · ")[0];
}

describe("route badge — trust zone comes from the backend", () => {
  it("renders Cloud for a cloud-served turn whose provider was removed", async () => {
    // THE blocking defect. The provider is gone from the registry, so the old
    // code fell through to "local" and painted this turn green. The stamp says
    // cloud; the stamp wins.
    seedTurn({
      provider_id: "deleted-openai",
      provider_name: null,
      base_url: null,
      zone: "cloud",
    });
    render(MainScreen);

    expect(await badgeZone()).toBe("Cloud");
  });

  it("renders Cloud even when a same-id LOCAL provider is what the store holds", async () => {
    // The provider id still resolves — to a local endpoint, because the user
    // repointed it after the turn ran. Re-deriving from live state would call
    // this local. The turn's own stamp says it egressed.
    seedTurn({
      provider_id: "local-llm",
      provider_name: "Local Llama",
      base_url: "http://127.0.0.1:11434/v1",
      zone: "cloud",
    });
    render(MainScreen);

    // Named "Local Llama" on purpose: the zone claim must be Cloud even
    // though the endpoint's own NAME says local.
    expect(await badgeZone()).toBe("Cloud");
  });

  it("renders Unknown — never Local — when no zone was stamped", async () => {
    // A row older than the stamp. The provider resolves, and it is LOCAL, so
    // every "just look it up" path would answer "Local" here. We don't know,
    // and we say so.
    seedTurn({
      provider_id: "local-llm",
      provider_name: "Local Llama",
      base_url: "http://127.0.0.1:11434/v1",
      zone: null,
    });
    render(MainScreen);

    // Not "Local", and not "Held" either — an unknown route is not a blocked
    // one, and the old chained-ternary label would have called it "Held".
    expect(await badgeZone()).toBe("Unknown route");
  });

  it("renders Unknown when the turn has no served_by block at all", async () => {
    seedTurn(null);
    render(MainScreen);

    expect(await badgeZone()).toBe("Unknown route");
  });

  it("renders Local for a locally-served turn", async () => {
    // The honest positive case still works — this is not a fix that just
    // stops saying "Local".
    seedTurn({
      provider_id: "local-llm",
      provider_name: "Local Llama",
      base_url: "http://127.0.0.1:11434/v1",
      zone: "local",
    });
    render(MainScreen);

    expect(await badgeZone()).toBe("Local");
  });

  it("will NOT call a pre-stamp privacy reroute 'Local'", async () => {
    // This test used to assert "Local" here, on the argument that `route_local`
    // is itself a persisted backend fact and `enforce_local_routing`
    // structurally proves its target was local+private. That argument does not
    // hold, and the test was blessing the behaviour rather than checking it:
    //
    //   - The backend does not make the inference either. On the `RouteLocal`
    //     branch, `process_message_inner` picks the endpoint and THEN computes
    //     `local_is_cloud = !is_private_endpoint(&local.base_url)` to stamp the
    //     row, because `find_or_start_local_provider` also reaches
    //     `models::runner::ensure_running`, which mints the bundled sidecar's
    //     provider without passing through the enforcer.
    //   - A zone-null row was written by a build older than the stamp. Proving
    //     it with today's routing code is proving it with code that did not run.
    //
    // "Local" is the reassuring answer, and the reassuring answer is exactly
    // the one this whole workstream forbids guessing.
    seedTurn(null, "route_local");
    render(MainScreen);

    expect(await badgeZone()).toBe("Unknown route");
  });

  it("still answers Cloud for a pre-stamp redact-and-send", async () => {
    // The asymmetry is deliberate, and it is the rule: a routing decision may
    // support the LESS reassuring claim only. The backend reaches `redact_send`
    // exclusively under `if is_cloud`, so that turn provably egressed; being
    // wrong in this direction can only over-warn.
    seedTurn(null, "redact_send");
    render(MainScreen);

    expect(await badgeZone()).toBe("Cloud");
  });

  it("explains the unknown state instead of leaving a bare chip", async () => {
    seedTurn({
      provider_id: "local-llm",
      provider_name: "Local Llama",
      base_url: "http://127.0.0.1:11434/v1",
      zone: null,
    });
    render(MainScreen);

    await badgeText();
    const explained = Array.from(
      document.body.querySelectorAll("[title]"),
    ).some((el) => /can't be confirmed/.test(el.getAttribute("title") ?? ""));
    expect(explained).toBe(true);
  });
});

describe("the 'Kept on your device' event bar is gated on the stamp too", () => {
  // The bar is the LOUDER of the two surfaces — a full-width green banner
  // saying the content never left the machine. It was rendered off
  // `routing_decision === "route_local"` alone, so a pre-stamp row got the
  // reassurance the badge had already stopped giving. Two surfaces, one fact:
  // both now read `served_by.zone`.

  it("says 'Kept on your machine' when the backend stamped the turn local", async () => {
    seedTurn(
      {
        provider_id: "local-llm",
        provider_name: "Local Llama",
        base_url: "http://127.0.0.1:11434/v1",
        zone: "local",
      },
      "route_local",
    );
    render(MainScreen);

    expect(await screen.findByText("Kept on your machine")).toBeInTheDocument();
  });

  it("will not claim a pre-stamp reroute stayed on the machine", async () => {
    seedTurn(null, "route_local");
    render(MainScreen);

    // The reroute is still surfaced — the user should know the gate moved
    // their turn — but without the claim we cannot back.
    expect(
      await screen.findByText("Rerouted — not confirmed local"),
    ).toBeInTheDocument();
    expect(screen.queryByText("Kept on your machine")).toBeNull();
    expect(document.body.textContent).toContain("can't be confirmed");
  });

  it("says the content left the machine when the stamp says cloud", async () => {
    // `route_local` + a cloud stamp is the contradiction the backend's own
    // `local_is_cloud` recomputation exists to catch. The stamp wins, and the
    // bar says so in words rather than showing a green padlock.
    seedTurn(
      {
        provider_id: "sidecar",
        provider_name: "Sidecar",
        base_url: "https://api.example.com/v1",
        zone: "cloud",
      },
      "route_local",
    );
    render(MainScreen);

    expect(
      await screen.findByText("Rerouted — not confirmed local"),
    ).toBeInTheDocument();
    expect(document.body.textContent).toContain("still left it");
  });
});

describe("pre-send indicators use the same trust-zone predicate", () => {
  // The composer's picker dot/tag and its Send button are claims about where
  // the NEXT turn will go. They legitimately read live config — but they must
  // read the same bit the backend stamps turns with (`is_private`, the base
  // URL), not `kind`, which is a user-typed label with no enforcement power.
  const PUBLIC_CUSTOM = [
    {
      id: "public-custom",
      // Kind "custom", so the old `kind === "cloud" ? cloud : local` rule
      // called this LOCAL — green dot, "on device" tag, green Send button —
      // for an endpoint on the public internet.
      name: "Someone else's server",
      base_url: "https://api.example.com/v1",
      kind: "custom",
      is_private: false,
      trusted_by_name: false,
      supports_native_tools: false,
    },
  ];

  beforeEach(async () => {
    resetProviders();
    invokeMock.mockImplementation(async (cmd: string) => {
      switch (cmd) {
        case "list_providers":
          return PUBLIC_CUSTOM;
        case "list_models":
          return ["some-model"];
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

  it("labels a public custom endpoint as cloud in the picker, not 'on device'", async () => {
    render(MainScreen);
    const openPicker = await screen.findByRole("button", {
      name: /thinking strength/i,
    });
    openPicker.click();

    await waitFor(() => {
      const group = Array.from(document.body.querySelectorAll("div")).find((d) =>
        /Someone else's server/.test(d.textContent ?? ""),
      );
      expect(group?.textContent).toContain("cloud");
      expect(group?.textContent).not.toContain("on device");
    });
  });

  it("does not offer to 'Send via local model' to a public custom endpoint", async () => {
    render(MainScreen);

    // Arm the composer on that endpoint.
    const openPicker = await screen.findByRole("button", {
      name: /thinking strength/i,
    });
    await fireEvent.click(openPicker);
    const option = await screen.findByRole("option");
    await fireEvent.click(option);

    // Leave Auto (which is honestly "privacy filter") and switch the binding
    // to Public, where the composer commits to a route before sending.
    const bindingBtn = screen.getByLabelText(
      "Conversation binding — click to switch",
    );
    for (let i = 0; i < 3; i++) {
      if (/public/i.test(bindingBtn.textContent ?? "")) break;
      await fireEvent.click(bindingBtn);
    }
    expect(bindingBtn.textContent?.toLowerCase()).toContain("public");

    const send = await screen.findByRole("button", { name: /^Send via/ });
    expect(send.getAttribute("aria-label") ?? send.textContent).toContain(
      "public model",
    );
    expect(send.getAttribute("aria-label") ?? send.textContent).not.toContain(
      "local model",
    );
  });
});

describe("RoutingBadge — the unknown state is visually distinct", () => {
  it("does not borrow the local (green) tone", () => {
    const { container: unknown } = render(RoutingBadge, { route: "unknown" });
    const unknownClass = unknown.firstElementChild!.className;
    expect(unknownClass).not.toContain("text-local");
    expect(unknownClass).not.toContain("bg-local");

    const { container: local } = render(RoutingBadge, { route: "local" });
    expect(local.firstElementChild!.className).not.toBe(unknownClass);
  });

  it("labels itself rather than falling back to a route name", () => {
    render(RoutingBadge, { route: "unknown" });
    expect(screen.getByText("Unknown route")).toBeInTheDocument();
  });
});
