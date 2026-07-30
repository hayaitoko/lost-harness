/// <reference types="vitest" />

/**
 * Self-update UI tests (round-2 item 3).
 *
 * Two things are worth pinning on the frontend side:
 *
 *  1. **The wire contract.** The Rust commands are hand-rolled, so the `args`
 *     envelope and the exact command names are a real regression surface —
 *     the same reason `approval.test.ts` exists. These tests mock the Tauri
 *     runtime and assert on the literal `invoke` payloads.
 *  2. **Nothing installs on its own.** Mounting the banner and letting the
 *     "update available" event fire must produce an OFFER and nothing else:
 *     no `install_update`, no `plugin:process|restart`. That is the frontend
 *     half of the spec's "no silent installs".
 *
 * The zero-egress-when-the-toggle-is-off guarantee is NOT tested here — it is
 * enforced in Rust (`updater::run_launch_check`) and proven in
 * `src-tauri/src/updater/tests.rs`, because the launch check runs before this
 * code exists.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup, waitFor } from "@testing-library/svelte";
import { tick } from "svelte";
import UpdateBanner from "$lib/design/components/UpdateBanner.svelte";
import {
  UPDATE_AVAILABLE_EVENT,
  getUpdateCheckEnabled,
  setUpdateCheckEnabled,
  checkForUpdate,
  installUpdate,
  relaunchApp,
  type UpdateInfo,
} from "$lib/api/tauri";
import { availableUpdate, clearAvailableUpdate, setAvailableUpdate } from "$lib/stores/update";

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

function emit(event: string, payload: unknown) {
  for (const handler of tauri.listeners.get(event) ?? []) handler({ payload });
}

beforeEach(() => {
  tauri.invoke.mockReset();
  tauri.invoke.mockResolvedValue(undefined);
  tauri.unlisten.mockReset();
  tauri.listeners.clear();
  clearAvailableUpdate();
  window.__TAURI_INTERNALS__ = {};
});

afterEach(() => {
  cleanup();
  clearAvailableUpdate();
  window.__TAURI_INTERNALS__ = undefined;
});

function anUpdate(over: Partial<UpdateInfo> = {}): UpdateInfo {
  return {
    version: "0.1.1",
    current_version: "0.1.0",
    notes: "A visible marker change.",
    date: "2026-07-30T00:00:00Z",
    ...over,
  };
}

// ── The wire contract ───────────────────────────────────────────────────────

describe("update IPC contract", () => {
  it("reads the toggle with a no-arg command", async () => {
    tauri.invoke.mockResolvedValue(false);
    await expect(getUpdateCheckEnabled()).resolves.toBe(false);
    expect(tauri.invoke).toHaveBeenCalledWith("get_update_check_enabled", {});
  });

  it("writes the toggle inside the `args` envelope Rust deserializes", async () => {
    await setUpdateCheckEnabled(false);
    // Tauri v2: struct params arrive nested under `args`, snake_case inside.
    // A flat `{ enabled }` is rejected at the IPC layer before the command body.
    expect(tauri.invoke).toHaveBeenCalledWith("set_update_check_enabled", {
      args: { enabled: false },
    });
  });

  it("routes the manual check through the app's own command, not the plugin", async () => {
    tauri.invoke.mockResolvedValue({ status: "available", ...anUpdate() });
    const result = await checkForUpdate();
    expect(result).toEqual({ status: "available", ...anUpdate() });
    expect(tauri.invoke).toHaveBeenCalledWith("check_for_update");
    // The webview has no `updater:*` ACL grant, so this must never appear.
    for (const [cmd] of tauri.invoke.mock.calls) {
      expect(cmd).not.toMatch(/^plugin:updater\|/);
    }
  });

  it("relaunches through the process plugin the capability actually grants", async () => {
    await relaunchApp();
    expect(tauri.invoke).toHaveBeenCalledWith("plugin:process|restart");
  });

  it("surfaces a refused install as a rejection rather than swallowing it", async () => {
    tauri.invoke.mockRejectedValue("signature verification failed");
    await expect(installUpdate()).rejects.toBe("signature verification failed");
  });
});

// ── Browser mode makes no calls at all ──────────────────────────────────────

describe("outside the Tauri shell", () => {
  beforeEach(() => {
    window.__TAURI_INTERNALS__ = undefined;
  });

  it("never invokes anything", async () => {
    await getUpdateCheckEnabled();
    await setUpdateCheckEnabled(true);
    await checkForUpdate();
    await installUpdate();
    await relaunchApp();
    expect(tauri.invoke).not.toHaveBeenCalled();
  });
});

// ── The banner: an offer, never an install ──────────────────────────────────

describe("UpdateBanner", () => {
  it("renders nothing until something has found an update", async () => {
    render(UpdateBanner);
    await tick();
    expect(screen.queryByTestId("update-banner")).toBeNull();
  });

  it("offers the update on the launch-check event WITHOUT installing it", async () => {
    render(UpdateBanner);
    await tick();

    emit(UPDATE_AVAILABLE_EVENT, anUpdate());
    await waitFor(() => expect(screen.getByTestId("update-banner")).toBeInTheDocument());

    expect(screen.getByTestId("update-version")).toHaveTextContent("Update available 0.1.1");
    // The whole point: an update was found and NOTHING was downloaded.
    expect(tauri.invoke).not.toHaveBeenCalled();
  });

  it("downloads only when the user clicks, then asks before restarting", async () => {
    render(UpdateBanner);
    await tick();
    emit(UPDATE_AVAILABLE_EVENT, anUpdate());
    await waitFor(() => expect(screen.getByTestId("update-install")).toBeInTheDocument());

    tauri.invoke.mockResolvedValue(undefined);
    await fireEvent.click(screen.getByTestId("update-install"));

    await waitFor(() => expect(screen.getByTestId("update-ready")).toBeInTheDocument());
    expect(tauri.invoke).toHaveBeenCalledWith("install_update");
    // Installed — and still not restarted. The restart is its own click.
    expect(tauri.invoke).not.toHaveBeenCalledWith("plugin:process|restart");

    await fireEvent.click(screen.getByTestId("update-relaunch"));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("plugin:process|restart"),
    );
  });

  it("says so plainly when the download is refused, and does not restart", async () => {
    render(UpdateBanner);
    await tick();
    emit(UPDATE_AVAILABLE_EVENT, anUpdate());
    await waitFor(() => expect(screen.getByTestId("update-install")).toBeInTheDocument());

    tauri.invoke.mockRejectedValue("signature verification failed");
    await fireEvent.click(screen.getByTestId("update-install"));

    await waitFor(() => expect(screen.getByTestId("update-error")).toBeInTheDocument());
    expect(screen.getByTestId("update-error")).toHaveTextContent(
      "signature verification failed",
    );
    expect(screen.queryByTestId("update-relaunch")).toBeNull();
    expect(tauri.invoke).not.toHaveBeenCalledWith("plugin:process|restart");
  });

  it("shows an update a manual check put in the store", async () => {
    render(UpdateBanner);
    await tick();
    setAvailableUpdate(anUpdate({ version: "0.2.0" }));
    await waitFor(() => expect(screen.getByTestId("update-version")).toBeInTheDocument());
    expect(screen.getByTestId("update-version")).toHaveTextContent("Update available 0.2.0");
  });

  it("clears the offer when dismissed", async () => {
    render(UpdateBanner);
    await tick();
    emit(UPDATE_AVAILABLE_EVENT, anUpdate());
    await waitFor(() => expect(screen.getByTestId("update-dismiss")).toBeInTheDocument());

    await fireEvent.click(screen.getByTestId("update-dismiss"));
    await waitFor(() => expect(screen.queryByTestId("update-banner")).toBeNull());

    let current: UpdateInfo | null = anUpdate();
    availableUpdate.subscribe((v) => (current = v))();
    expect(current).toBeNull();
    expect(tauri.invoke).not.toHaveBeenCalled();
  });
});
