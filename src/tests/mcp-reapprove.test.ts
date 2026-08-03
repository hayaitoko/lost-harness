/// <reference types="vitest" />

/**
 * MCP pin-refusal surfacing + Re-approve recovery (PROGRESS-MAP open
 * follow-up #2).
 *
 * The backend fail-closes any stdio MCP server whose pinned executable
 * identity no longer matches what was approved (or was never measured —
 * pre-pinning rows). That refusal used to die in the tracing log while the
 * pane showed a bare "stopped". This mounts the REAL `Settings.svelte` MCP
 * pane against fixture rows carrying the typed `pin_refusal` the IPC layer
 * now returns, and proves:
 *
 *  1. a refused server renders as "blocked" with the old-vs-new identity and
 *     a Re-approve action; a pre-pinning row renders the "never verified"
 *     state; a healthy row shows neither;
 *  2. Re-approve is a TWO-CLICK explicit action — the first click only arms
 *     the exact target and calls nothing (the fail-closed property: nothing
 *     ever re-pins without a deliberate human confirmation);
 *  3. the confirmed click calls `reapproveMcpServer` with the server id and
 *     the returned (running, refusal-free) server replaces the row.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, waitFor } from "@testing-library/svelte";
import Settings from "$lib/design/screens/Settings.svelte";
import type { McpServer } from "$lib/api/tauri";

const mocks = vi.hoisted(() => ({
  listMcpServers: vi.fn<() => Promise<unknown[]>>(async () => []),
  reapproveMcpServer: vi.fn<(id: string) => Promise<unknown>>(async () => null),
}));

vi.mock("$lib/api/tauri", async (importOriginal) => {
  const actual = await importOriginal<typeof import("$lib/api/tauri")>();
  return {
    ...actual,
    listMcpServers: mocks.listMcpServers,
    reapproveMcpServer: mocks.reapproveMcpServer,
  };
});

const HEALTHY: McpServer = {
  id: "srv-ok",
  name: "github",
  command: "/usr/local/bin/github-mcp",
  args: [],
  tier: "remote",
  trusted_read_only: false,
  enabled: true,
  running: true,
  tools: ["mcp__github__search"],
  pin_refusal: null,
};

// The Node-upgrade / package-update shape: interpreter or script content
// changed underneath the stored pin.
const CHANGED: McpServer = {
  id: "srv-changed",
  name: "files",
  command: "node",
  args: ["/opt/files/server.js"],
  tier: "local",
  trusted_read_only: false,
  enabled: true,
  running: false,
  tools: [],
  pin_refusal: {
    kind: "invocation_changed",
    actual_path: "/usr/local/bin/node",
    approved_pin: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    actual_pin: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  },
};

// A row registered before the hash-pinning build existed.
const UNPINNED: McpServer = {
  id: "srv-legacy",
  name: "legacy",
  command: "/usr/local/bin/legacy-mcp",
  args: [],
  tier: "remote",
  trusted_read_only: false,
  enabled: true,
  running: false,
  tools: [],
  pin_refusal: { kind: "unpinned" },
};

const REAPPROVED: McpServer = {
  ...CHANGED,
  running: true,
  tools: ["mcp__files__read"],
  pin_refusal: null,
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

async function openMcpPane() {
  const utils = render(Settings);
  await fireEvent.click(utils.getByRole("button", { name: "MCP servers" }));
  // The mocked listMcpServers resolves asynchronously; wait for a row.
  await utils.findByText("files");
  return utils;
}

describe("Settings — MCP pin-refusal state", () => {
  it("renders blocked + the old/new identity for a changed binary, and the pre-pinning state", async () => {
    mocks.listMcpServers.mockResolvedValue([HEALTHY, CHANGED, UNPINNED]);
    const { getByText, getAllByText, queryAllByRole } = await openMcpPane();

    // The refused servers are "blocked", not a bare "stopped" — and the
    // healthy one is running. No row says "stopped".
    expect(getAllByText("blocked")).toHaveLength(2);
    expect(getByText("running")).toBeInTheDocument();

    // The changed-binary notice names the state and shows BOTH pins
    // (truncated), so the user can see the identity actually moved.
    expect(getByText("Binary changed since approval.")).toBeInTheDocument();
    expect(
      getByText(/pin aaaaaaaaaaaa… → bbbbbbbbbbbb…/),
    ).toBeInTheDocument();

    // The pre-pinning row gets its own honest state.
    expect(getByText("Never verified.")).toBeInTheDocument();
    expect(
      getByText(/registered before executable verification existed/),
    ).toBeInTheDocument();

    // Exactly the two refused servers offer Re-approve — never the healthy one.
    expect(queryAllByRole("button", { name: "Re-approve" })).toHaveLength(2);
  });

  it("re-approves only on the second, armed click — and swaps in the recovered server", async () => {
    mocks.listMcpServers.mockResolvedValue([HEALTHY, CHANGED]);
    mocks.reapproveMcpServer.mockResolvedValue(REAPPROVED);
    const { getByRole, getAllByText, queryByText, findByRole } = await openMcpPane();

    // First click ARMS: it names the exact command about to be re-trusted and
    // must not call the backend (a silent auto-repin here would defeat the
    // fail-closed property the pin gate exists for).
    await fireEvent.click(getByRole("button", { name: "Re-approve" }));
    expect(mocks.reapproveMcpServer).not.toHaveBeenCalled();
    const armed = await findByRole("button", {
      name: "Trust node as it is now?",
    });

    // Second click is the explicit re-trust.
    await fireEvent.click(armed);
    expect(mocks.reapproveMcpServer).toHaveBeenCalledTimes(1);
    expect(mocks.reapproveMcpServer).toHaveBeenCalledWith("srv-changed");

    // The recovered server replaces the row: running, warning gone.
    await waitFor(() => {
      expect(getAllByText("running")).toHaveLength(2);
    });
    expect(queryByText("blocked")).not.toBeInTheDocument();
    expect(queryByText("Binary changed since approval.")).not.toBeInTheDocument();
  });
});
