/// <reference types="vitest" />

/**
 * ApprovalDialog component tests.
 *
 * The approval gate is a security boundary: it is the last place a human sees
 * a tool call before the parked Rust dispatch is released. These tests mount
 * the REAL `ApprovalDialog.svelte` and assert on its rendered DOM and on the
 * exact IPC payload it sends — nothing here re-derives the component's rules.
 *
 * The seam is the Tauri runtime itself: `@tauri-apps/api/event`'s `listen` and
 * `@tauri-apps/api/core`'s `invoke` are mocked and `window.__TAURI_INTERNALS__`
 * is set, so `$lib/api/tauri` takes its real Tauri branch. That means these
 * tests also pin the wire contract (`resolve_tool_approval` + the `args`
 * wrapper) that the Rust side deserializes.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/svelte";
import { tick } from "svelte";
import ApprovalDialog from "$lib/components/ApprovalDialog.svelte";
import {
  TOOL_APPROVAL_REQUEST_EVENT,
  type RiskClass,
  type ToolApprovalRequest,
} from "$lib/api/tauri";

// ── Tauri runtime seam ──────────────────────────────────────────────────────

type EventHandler = (event: { payload: unknown }) => void;

const tauri = vi.hoisted(() => ({
  invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
  unlisten: vi.fn(),
  listeners: new Map<string, EventHandler[]>(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: tauri.invoke,
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: async (event: string, handler: EventHandler) => {
    const handlers = tauri.listeners.get(event) ?? [];
    handlers.push(handler);
    tauri.listeners.set(event, handlers);
    return tauri.unlisten;
  },
}));

beforeEach(() => {
  tauri.invoke.mockReset();
  tauri.invoke.mockResolvedValue(true);
  tauri.unlisten.mockReset();
  tauri.listeners.clear();
  // Make `isTauri()` true so the bridge uses its real invoke/listen path.
  window.__TAURI_INTERNALS__ = {};
});

afterEach(() => {
  cleanup();
  window.__TAURI_INTERNALS__ = undefined;
});

// ── Helpers ─────────────────────────────────────────────────────────────────

function makeRequest(over: Partial<ToolApprovalRequest> = {}): ToolApprovalRequest {
  return {
    id: "req-1",
    conversation_id: "conv-1",
    tool_name: "write_file",
    command: 'write_file({ path: "notes.txt" })',
    prompt: "The agent wants to write notes.txt",
    by: "permission",
    fingerprint: "fp-1",
    risk: "write",
    destination: null,
    ...over,
  };
}

/** Mounts the dialog and waits for its `onMount` listener registration. */
async function mountDialog() {
  const utils = render(ApprovalDialog);
  await tick();
  return utils;
}

/** Fires a backend `tool:approval_request` event at the mounted component. */
async function emitRequest(...requests: ToolApprovalRequest[]) {
  const handlers = tauri.listeners.get(TOOL_APPROVAL_REQUEST_EVENT) ?? [];
  expect(handlers.length).toBeGreaterThan(0);
  for (const req of requests) {
    for (const handler of handlers) handler({ payload: req });
  }
  await tick();
}

/** Mounts and immediately delivers one request; returns the visible dialog. */
async function showRequest(over: Partial<ToolApprovalRequest> = {}) {
  await mountDialog();
  const request = makeRequest(over);
  await emitRequest(request);
  return { request, dialog: screen.getByTestId("tool-approval-dialog") };
}

const buttonLabels = () =>
  Array.from(screen.getByTestId("tool-approval-dialog").querySelectorAll("button")).map(
    (b) => b.textContent?.trim(),
  );

/** The IPC payload of the Nth `resolve_tool_approval` invoke. */
function resolveCall(index = 0) {
  const calls = tauri.invoke.mock.calls.filter((c) => c[0] === "resolve_tool_approval");
  return calls[index];
}

// ── Rendering ───────────────────────────────────────────────────────────────

describe("ApprovalDialog — rendering", () => {
  it("renders nothing until a request arrives", async () => {
    await mountDialog();
    expect(screen.queryByTestId("tool-approval-dialog")).toBeNull();
  });

  it("subscribes to the tool:approval_request event on mount", async () => {
    await mountDialog();
    expect(tauri.listeners.get(TOOL_APPROVAL_REQUEST_EVENT)?.length).toBe(1);
  });

  it("shows the tool name and the exact command being approved", async () => {
    await showRequest({
      tool_name: "exec",
      command: 'exec({ command: "rm -rf /tmp/cache" })',
    });

    expect(screen.getByText("exec")).toBeInTheDocument();
    expect(screen.getByTestId("approval-command")).toHaveTextContent(
      'exec({ command: "rm -rf /tmp/cache" })',
    );
  });

  it("renders the command as text, never as markup", async () => {
    await showRequest({ command: 'write_file({ body: "<img src=x onerror=alert(1)>" })' });

    const pre = screen.getByTestId("approval-command");
    expect(pre.querySelector("img")).toBeNull();
    expect(pre.textContent).toContain("<img src=x onerror=alert(1)>");
  });

  it("is an accessible modal dialog", async () => {
    const { dialog } = await showRequest();
    expect(dialog).toHaveAttribute("role", "dialog");
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(screen.getByRole("heading", { name: "Allow this tool to run?" })).toBeInTheDocument();
  });

  it("shows the prompt text supplied by the backend", async () => {
    await showRequest({ prompt: "Agent wants to append to the audit log" });
    expect(screen.getByText("Agent wants to append to the audit log")).toBeInTheDocument();
  });
});

// ── Risk presentation ───────────────────────────────────────────────────────

describe("ApprovalDialog — risk presentation", () => {
  it.each([
    ["safe", "Safe"],
    ["write", "Write"],
    ["external", "External"],
    ["dangerous", "Dangerous"],
  ] as Array<[RiskClass, string]>)("labels %s requests as %s", async (risk, label) => {
    await showRequest({ risk, destination: risk === "external" ? "https://example.com" : null });
    expect(screen.getByTestId("approval-risk-badge")).toHaveTextContent(label);
  });

  it("presents a dangerous action with the destructive (red) treatment", async () => {
    await showRequest({ risk: "dangerous", tool_name: "exec", command: "exec({})" });

    const badge = screen.getByTestId("approval-risk-badge");
    expect(badge).toHaveTextContent("Dangerous");
    expect(badge.className).toMatch(/red/);
  });

  it("falls back to Unknown for a risk class this frontend does not know", async () => {
    // Fail-closed: a newer backend risk variant must not silently render as a
    // low-risk one, and must not unlock standing grants.
    await showRequest({ risk: "catastrophic" as RiskClass });

    expect(screen.getByTestId("approval-risk-badge")).toHaveTextContent("Unknown");
    expect(screen.queryByTestId("approval-session")).toBeNull();
    expect(screen.queryByTestId("approval-always")).toBeNull();
  });

  it("surfaces the egress destination for an external request", async () => {
    await showRequest({
      risk: "external",
      tool_name: "web_fetch",
      command: 'web_fetch({ url: "https://api.example.com/v1" })',
      destination: "https://api.example.com/v1",
    });

    const destination = screen.getByTestId("approval-destination");
    expect(destination).toHaveTextContent("Sends to");
    expect(destination).toHaveTextContent("https://api.example.com/v1");
  });

  it("omits the destination line when the call is not egress", async () => {
    await showRequest({ risk: "write", destination: null });
    expect(screen.queryByTestId("approval-destination")).toBeNull();
  });
});

// ── Grant matrix, as actually rendered ──────────────────────────────────────

describe("ApprovalDialog — offered grants by risk class", () => {
  it("dangerous offers only Deny and Allow once", async () => {
    await showRequest({ risk: "dangerous" });
    expect(buttonLabels()).toEqual(["Deny", "Allow once"]);
  });

  it("external offers only Deny and Allow once", async () => {
    await showRequest({ risk: "external", destination: "https://example.com" });
    expect(buttonLabels()).toEqual(["Deny", "Allow once"]);
  });

  it("write offers Deny, Always allow, Allow for this session, Allow once", async () => {
    await showRequest({ risk: "write" });
    expect(buttonLabels()).toEqual([
      "Deny",
      "Always allow",
      "Allow for this session",
      "Allow once",
    ]);
  });

  it("safe offers a session grant but never a persisted always grant", async () => {
    await showRequest({ risk: "safe" });
    expect(buttonLabels()).toEqual(["Deny", "Allow for this session", "Allow once"]);
  });
});

// ── Decisions sent to the backend ───────────────────────────────────────────

describe("ApprovalDialog — decisions", () => {
  it("Allow once approves this one call only", async () => {
    const { request } = await showRequest({ risk: "dangerous" });

    await fireEvent.click(screen.getByTestId("approval-once"));

    expect(resolveCall()).toEqual([
      "resolve_tool_approval",
      {
        args: {
          id: request.id,
          decision: "approve",
          scope: "once",
          target: "action",
          pattern: "*",
        },
      },
    ]);
  });

  it("Deny sends a denial for the shown request", async () => {
    const { request } = await showRequest({ risk: "write" });

    await fireEvent.click(screen.getByTestId("approval-deny"));

    expect(resolveCall()).toEqual([
      "resolve_tool_approval",
      {
        args: {
          id: request.id,
          decision: "deny",
          scope: "once",
          target: "action",
          pattern: "*",
        },
      },
    ]);
  });

  it("Allow for this session grants the whole tool for the session", async () => {
    await showRequest({ risk: "safe" });

    await fireEvent.click(screen.getByTestId("approval-session"));

    expect(resolveCall()?.[1]).toMatchObject({
      args: { decision: "approve", scope: "session", target: "tool" },
    });
  });

  it("Always allow persists a whole-tool rule", async () => {
    await showRequest({ risk: "write" });

    await fireEvent.click(screen.getByTestId("approval-always"));

    expect(resolveCall()?.[1]).toMatchObject({
      args: { decision: "approve", scope: "always", target: "tool", pattern: "*" },
    });
  });

  it("Escape denies the shown request (safe default)", async () => {
    const { request } = await showRequest({ risk: "dangerous" });

    await fireEvent.keyDown(window, { key: "Escape" });

    expect(resolveCall()?.[1]).toMatchObject({
      args: { id: request.id, decision: "deny" },
    });
  });

  it("Escape with no pending request sends nothing", async () => {
    await mountDialog();
    await fireEvent.keyDown(window, { key: "Escape" });
    expect(tauri.invoke).not.toHaveBeenCalled();
  });

  it("dismisses the dialog once the decision is delivered", async () => {
    await showRequest({ risk: "write" });

    await fireEvent.click(screen.getByTestId("approval-deny"));
    await tick();

    expect(screen.queryByTestId("tool-approval-dialog")).toBeNull();
  });

  it("dismisses a stale request the backend no longer knows about", async () => {
    tauri.invoke.mockResolvedValue(false);
    await showRequest({ risk: "safe" });

    await fireEvent.click(screen.getByTestId("approval-once"));
    await tick();

    expect(screen.queryByTestId("tool-approval-dialog")).toBeNull();
  });

  it("dismisses the request even if the IPC call throws", async () => {
    tauri.invoke.mockRejectedValue(new Error("ipc down"));
    await showRequest({ risk: "write" });

    await fireEvent.click(screen.getByTestId("approval-deny"));
    await tick();

    expect(screen.queryByTestId("tool-approval-dialog")).toBeNull();
  });
});

// ── Double-submit guard ─────────────────────────────────────────────────────

describe("ApprovalDialog — double-submit guard", () => {
  it("a double click resolves the request exactly once", async () => {
    let release: (v: boolean) => void = () => {};
    tauri.invoke.mockReturnValue(
      new Promise<boolean>((resolve) => {
        release = resolve;
      }),
    );

    await showRequest({ risk: "dangerous" });

    // Two synchronous clicks, before Svelte can flush `disabled={resolving}`.
    const once = screen.getByTestId("approval-once") as HTMLButtonElement;
    once.click();
    once.click();
    await tick();

    expect(tauri.invoke.mock.calls.filter((c) => c[0] === "resolve_tool_approval")).toHaveLength(
      1,
    );

    release(true);
  });

  it("disables the buttons while a decision is in flight", async () => {
    let release: (v: boolean) => void = () => {};
    tauri.invoke.mockReturnValue(
      new Promise<boolean>((resolve) => {
        release = resolve;
      }),
    );

    await showRequest({ risk: "write" });
    await fireEvent.click(screen.getByTestId("approval-deny"));

    expect(screen.getByTestId("approval-deny")).toBeDisabled();
    expect(screen.getByTestId("approval-once")).toBeDisabled();

    release(true);
  });
});

// ── Queue ───────────────────────────────────────────────────────────────────

describe("ApprovalDialog — request queue", () => {
  it("shows the head of the queue and counts the ones still waiting", async () => {
    await mountDialog();
    await emitRequest(
      makeRequest({ id: "req-1", tool_name: "write_file" }),
      makeRequest({ id: "req-2", tool_name: "exec", risk: "dangerous" }),
      makeRequest({ id: "req-3", tool_name: "web_fetch", risk: "external" }),
    );

    expect(screen.getByText("write_file")).toBeInTheDocument();
    expect(screen.getByText("2 more waiting")).toBeInTheDocument();
  });

  it("advances FIFO, answering each request with its own id", async () => {
    await mountDialog();
    await emitRequest(
      makeRequest({ id: "req-1", tool_name: "write_file", risk: "write" }),
      makeRequest({ id: "req-2", tool_name: "exec", risk: "dangerous" }),
    );

    await fireEvent.click(screen.getByTestId("approval-once"));
    await tick();

    // Second request is now the head — and it is a dangerous one, so the
    // session/always buttons are gone even though the first request had them.
    expect(screen.getByText("exec")).toBeInTheDocument();
    expect(buttonLabels()).toEqual(["Deny", "Allow once"]);
    expect(screen.queryByText(/more waiting/)).toBeNull();

    await fireEvent.click(screen.getByTestId("approval-deny"));
    await tick();

    expect(screen.queryByTestId("tool-approval-dialog")).toBeNull();
    expect(resolveCall(0)?.[1]).toMatchObject({ args: { id: "req-1", decision: "approve" } });
    expect(resolveCall(1)?.[1]).toMatchObject({ args: { id: "req-2", decision: "deny" } });
  });

  it("a request arriving mid-decision is not lost", async () => {
    await mountDialog();
    await emitRequest(makeRequest({ id: "req-1", risk: "write" }));

    await fireEvent.click(screen.getByTestId("approval-deny"));
    await emitRequest(makeRequest({ id: "req-2", tool_name: "exec", risk: "dangerous" }));

    expect(screen.getByTestId("tool-approval-dialog")).toBeInTheDocument();
    expect(screen.getByText("exec")).toBeInTheDocument();
  });
});
