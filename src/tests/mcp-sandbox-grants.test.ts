/// <reference types="vitest" />

/**
 * MCP stdio sandbox grants (round-4 — the containment half H-07 deferred).
 *
 * The backend now spawns every stdio MCP child deny-default: it may read what
 * it needs to run plus one private scratch folder, and NOTHING else — no
 * network, none of the user's files — unless the user grants it at
 * registration. This mounts the REAL `Settings.svelte` MCP pane and proves the
 * grants are actually reachable and honest at the UI layer:
 *
 *  1. a registered server shows what its child can reach ("confined" when
 *     nothing is granted) — a grant the user cannot see is one they cannot
 *     revoke;
 *  2. registering with the grant controls untouched sends the DENY-DEFAULT to
 *     the backend (no network, no paths) — the failure mode this guards is a
 *     form that quietly re-opens the box;
 *  3. ticking network and typing paths sends exactly those grants, one
 *     absolute path per line;
 *  4. an HTTP endpoint never sends grants at all — it has no local child, and
 *     the backend refuses them.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, waitFor } from "@testing-library/svelte";
import Settings from "$lib/design/screens/Settings.svelte";
import type { McpServer } from "$lib/api/tauri";

const mocks = vi.hoisted(() => ({
  listMcpServers: vi.fn<() => Promise<unknown[]>>(async () => []),
  registerMcpServer: vi.fn<(s: unknown) => Promise<unknown>>(async () => null),
}));

vi.mock("$lib/api/tauri", async (importOriginal) => {
  const actual = await importOriginal<typeof import("$lib/api/tauri")>();
  return {
    ...actual,
    listMcpServers: mocks.listMcpServers,
    registerMcpServer: mocks.registerMcpServer,
  };
});

const CONFINED: McpServer = {
  id: "srv-confined",
  name: "confined-server",
  command: "/usr/local/bin/mcp-a",
  args: [],
  tier: "remote",
  trusted_read_only: false,
  enabled: true,
  running: true,
  tools: ["mcp__confined_server__a"],
  pin_refusal: null,
  network_access: false,
  read_paths: [],
  write_paths: [],
};

const GRANTED: McpServer = {
  id: "srv-granted",
  name: "granted-server",
  command: "/usr/local/bin/mcp-b",
  args: [],
  tier: "local",
  trusted_read_only: false,
  enabled: true,
  running: true,
  tools: ["mcp__granted_server__b"],
  pin_refusal: null,
  network_access: true,
  read_paths: ["/Users/you/Documents/notes"],
  write_paths: ["/Users/you/Documents/scratch"],
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

async function openMcpPane() {
  const utils = render(Settings);
  await fireEvent.click(utils.getByRole("button", { name: "MCP servers" }));
  await waitFor(() => expect(mocks.listMcpServers).toHaveBeenCalled());
  return utils;
}

/** Fill the add-a-server form and drive its two-click confirm. */
async function registerThroughTheForm(
  utils: Awaited<ReturnType<typeof openMcpPane>>,
  fill: () => Promise<void>,
) {
  await fireEvent.input(utils.getByPlaceholderText("Name (e.g. github)"), {
    target: { value: "srv" },
  });
  await fill();
  // First click only ARMS (nothing is registered without a deliberate second).
  await fireEvent.click(utils.getByRole("button", { name: "Register" }));
  expect(mocks.registerMcpServer).not.toHaveBeenCalled();
  const armed = await utils.findByRole("button", { name: /Confirm$/ });
  await fireEvent.click(armed);
  await waitFor(() => expect(mocks.registerMcpServer).toHaveBeenCalledTimes(1));
  return mocks.registerMcpServer.mock.calls[0][0] as Record<string, unknown>;
}

describe("Settings — MCP sandbox grants", () => {
  it("shows what each registered server's child can actually reach", async () => {
    mocks.listMcpServers.mockResolvedValue([CONFINED, GRANTED]);
    const { findByText, getByText } = await openMcpPane();

    // Nothing granted ⇒ the deny-default is stated, not left blank.
    await findByText("confined-server");
    expect(getByText("confined")).toBeInTheDocument();
    // Granted ⇒ both kinds of hole are named.
    expect(getByText("network + 2 paths")).toBeInTheDocument();
  });

  it("registers with the deny-default when the grant controls are untouched", async () => {
    const utils = await openMcpPane();
    const sent = await registerThroughTheForm(utils, async () => {
      await fireEvent.input(
        utils.getByPlaceholderText("Command (e.g. /usr/local/bin/my-mcp-server)"),
        { target: { value: "/usr/local/bin/my-mcp-server" } },
      );
    });
    expect(sent.network_access).toBe(false);
    expect(sent.read_paths).toEqual([]);
    expect(sent.write_paths).toEqual([]);
  });

  it("sends exactly the grants the user ticked and typed", async () => {
    const utils = await openMcpPane();
    const sent = await registerThroughTheForm(utils, async () => {
      await fireEvent.input(
        utils.getByPlaceholderText("Command (e.g. /usr/local/bin/my-mcp-server)"),
        { target: { value: "/usr/local/bin/my-mcp-server" } },
      );
      await fireEvent.click(utils.getByRole("switch", { name: /Allow network access/i }));
      await fireEvent.input(
        utils.getByPlaceholderText("/Users/you/Documents/notes"),
        { target: { value: "  /Users/you/Documents/notes  \n\n/Users/you/Desktop/refs\n" } },
      );
      await fireEvent.input(
        utils.getByPlaceholderText("/Users/you/Documents/scratch"),
        { target: { value: "/Users/you/Documents/scratch" } },
      );
    });
    expect(sent.network_access).toBe(true);
    // Blank lines dropped, each path trimmed — the shape the backend validates.
    expect(sent.read_paths).toEqual([
      "/Users/you/Documents/notes",
      "/Users/you/Desktop/refs",
    ]);
    expect(sent.write_paths).toEqual(["/Users/you/Documents/scratch"]);
  });

  it("never sends grants for a Streamable HTTP endpoint (it has no local child)", async () => {
    const utils = await openMcpPane();
    // Switching to HTTP hides the grant controls entirely.
    await fireEvent.change(utils.getByRole("combobox", { name: /Transport/ }), {
      target: { value: "http" },
    });
    await waitFor(() =>
      expect(utils.queryByPlaceholderText("/Users/you/Documents/notes")).toBeNull(),
    );
    const sent = await registerThroughTheForm(utils, async () => {
      await fireEvent.input(
        utils.getByPlaceholderText("Endpoint (e.g. https://example.com/mcp)"),
        { target: { value: "https://example.com/mcp" } },
      );
    });
    expect(sent.network_access).toBe(false);
    expect(sent.read_paths).toEqual([]);
    expect(sent.write_paths).toEqual([]);
  });
});
