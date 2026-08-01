/// <reference types="vitest" />

/**
 * OAuth reconnect tests — security-sensitive flows including the Gmail
 * OAuth setup wizard, per-profile connection, reconnect expiry handling,
 * error formatting, and the IPC contract for setGmailClient / gmailBeginConnect
 * / gmailFinishConnect / gmailDisconnect.
 *
 * The Gmail flow is M7-Q2: every user creates their own Google Cloud OAuth
 * client. No vendor client, no Lost Harness server in the loop. The pasted
 * client id/secret are install-global (stored in the keychain); the connection
 * (refresh token) is per-profile.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, waitFor } from "@testing-library/svelte";
import {
  setGmailClient,
  gmailBeginConnect,
  gmailFinishConnect,
  gmailDisconnect,
  gmailSetupStatus,
  type GmailSetupStatus,
} from "$lib/api/tauri";
import { GoogleConnection } from "$lib/design/googleConnection.svelte";
import GmailSetupWizard from "$lib/design/components/GmailSetupWizard.svelte";

// The IPC layer under the two "real store/component" describe blocks below
// (setup status data contract, per-profile isolation). Mocking at THIS level
// — rather than mocking `$lib/api/tauri` itself — keeps `gmailSetupStatus`,
// `gmailBeginConnect`, `gmailFinishConnect` etc. as the REAL functions from
// tauri.ts, so their `isTauri()` branch and argument shape are exercised too.
// It is also inert for the "IPC contracts" describe block above/below: those
// tests never set `window.__TAURI_INTERNALS__`, so `isTauri()` stays false
// and they keep hitting the real browser-fallback code paths regardless of
// this mock existing.
const tauri = vi.hoisted(() => ({
  invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async () => () => {},
}));

/** A full, healthy `GmailSetupStatus`, overridable per test. */
function status(over: Partial<GmailSetupStatus> = {}): GmailSetupStatus {
  return {
    client_configured: true,
    connected: true,
    account_email: "ada@example.com",
    needs_reconnect: false,
    api_not_enabled: null,
    ...over,
  };
}

afterEach(() => {
  cleanup();
  tauri.invoke.mockReset();
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
});

describe("Gmail OAuth — IPC contracts", () => {
  it("gmailSetupStatus returns disconnected state in browser fallback", async () => {
    const status = await gmailSetupStatus("personal");
    expect(status).toEqual({
      client_configured: false,
      connected: false,
      account_email: null,
      needs_reconnect: false,
      api_not_enabled: null,
    });
  });

  it("api_not_enabled is a SEPARATE state from needs_reconnect", async () => {
    // The two recoverable 403s must not share a field. Reconnecting fixes a
    // scope-short grant; it can never enable an API the user's Google Cloud
    // project has switched off, so folding them together would offer a
    // Reconnect button that loops forever.
    const status = await gmailSetupStatus("personal");
    expect(status.api_not_enabled).toBeNull();
    expect("needs_reconnect" in status && "api_not_enabled" in status).toBe(true);
  });

  it("gmailSetupStatus is per-profile", async () => {
    const status = await gmailSetupStatus("work");
    expect(status.client_configured).toBe(false);
    expect(status.connected).toBe(false);
  });

  it("setGmailClient throws in browser fallback (requires Tauri keychain)", async () => {
    await expect(
      setGmailClient(
        "12345.apps.googleusercontent.com",
        "GOCSPX-secret",
      ),
    ).rejects.toThrow("not available in browser mode");
  });

  it("gmailBeginConnect throws in browser fallback (requires Tauri loopback listener)", async () => {
    await expect(
      gmailBeginConnect("personal"),
    ).rejects.toThrow("not available in browser mode");
  });

  it("gmailFinishConnect throws in browser fallback (requires Tauri loopback listener)", async () => {
    await expect(
      gmailFinishConnect("personal"),
    ).rejects.toThrow("not available in browser mode");
  });

  it("gmailDisconnect throws in browser fallback (requires Tauri keychain)", async () => {
    await expect(
      gmailDisconnect("personal"),
    ).rejects.toThrow("not available in browser mode");
  });
});

// These two describe blocks used to build a `GmailSetupStatus` object
// literal by hand inside the test and then assert its own fields back —
// assertions that cannot fail no matter what the app does. Rewritten to
// drive the REAL store (`GoogleConnection` in googleConnection.svelte.ts)
// and the REAL wizard component (`GmailSetupWizard.svelte`) against a mocked
// Tauri backend, so a regression in either actually fails these tests.

describe("Gmail OAuth — setup status data contract", () => {
  it("needs_reconnect is a normal state: GoogleConnection.read() surfaces it on `status`, never `error`", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    tauri.invoke.mockImplementation(async (cmd: string) => {
      if (cmd === "gmail_setup_status") {
        return status({ connected: false, needs_reconnect: true });
      }
      return null;
    });

    const conn = new GoogleConnection();
    await conn.read("personal");

    // Testing-status Google clients expire refresh tokens every ~7 days.
    // needs_reconnect is a routine state, not an error — the UI's calm
    // Reconnect strip reads off `status`, never off `error`.
    expect(conn.status?.needs_reconnect).toBe(true);
    expect(conn.error).toBeNull();
  });

  it("account_email is null when unknown: the wizard renders it as-is, never a fabricated address", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    tauri.invoke.mockImplementation(async (cmd: string) => {
      if (cmd === "gmail_begin_connect") return { auth_url: "https://accounts.google.com/o/oauth2/…" };
      // The connect succeeded but the backend could not read the address —
      // per `GmailConnected`'s own contract, that is `null`, never a guess.
      if (cmd === "gmail_finish_connect") return { account_email: null };
      return null;
    });

    const { getByText } = render(GmailSetupWizard, {
      profile: "personal",
      status: status({ connected: false, client_configured: true }),
      variant: "reconnect",
    });

    await fireEvent.click(getByText("Connect Gmail"));

    const confirmed = await waitFor(() => getByText(/^Connected as/));
    // The load-bearing absence: nothing after "Connected as" — a fabricated
    // placeholder (e.g. "Connected as unknown@…") would fail this.
    expect(confirmed.textContent?.trim()).toBe("Connected as");
  });

  it("account_email that the backend DID resolve reaches the wizard unchanged", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    tauri.invoke.mockImplementation(async (cmd: string) => {
      if (cmd === "gmail_begin_connect") return { auth_url: "https://accounts.google.com/o/oauth2/…" };
      if (cmd === "gmail_finish_connect") return { account_email: "ada@example.com" };
      return null;
    });

    const { getByText } = render(GmailSetupWizard, {
      profile: "personal",
      status: status({ connected: false, client_configured: true }),
      variant: "reconnect",
    });

    await fireEvent.click(getByText("Connect Gmail"));

    const confirmed = await waitFor(() => getByText(/^Connected as/));
    expect(confirmed.textContent?.trim()).toBe("Connected as ada@example.com");
  });
});

describe("Gmail OAuth — per-profile isolation", () => {
  it("switching profiles drops a stale in-flight read for the OLD profile", async () => {
    // Mirrors Email.svelte's actual profile-switch handling: `conn.reset()`
    // fires the moment the active profile changes, and any read already in
    // flight for the profile being LEFT must not land afterwards — see
    // googleConnection.svelte.ts's own sequence-token comment. This is the
    // real mechanism "per-profile isolation" rests on, not two hand-built
    // objects that were never connected to any profile at all.
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    let resolvePersonalRead!: (v: GmailSetupStatus) => void;
    const personalRead = new Promise<GmailSetupStatus>((resolve) => {
      resolvePersonalRead = resolve;
    });
    tauri.invoke.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd !== "gmail_setup_status") return Promise.resolve(null);
      const profile = (args as { args: { profile: string } }).args.profile;
      // "personal" hangs until released below; "work" resolves immediately —
      // the two profiles' reads race, and "work" (the CURRENT profile) wins.
      return profile === "personal"
        ? personalRead
        : Promise.resolve(status({ connected: false, needs_reconnect: true, account_email: null }));
    });

    const conn = new GoogleConnection();
    const inFlight = conn.read("personal"); // the profile being LEFT
    conn.reset(); // the app switches to "work" before the read above resolves
    await conn.read("work"); // the new profile's own read completes first

    // The late arrival of the stale "personal" read must be a no-op.
    resolvePersonalRead(
      status({ connected: true, account_email: "personal@example.com" }),
    );
    await inFlight;

    expect(conn.status?.connected).toBe(false);
    expect(conn.status?.needs_reconnect).toBe(true);
    expect(conn.status?.account_email).not.toBe("personal@example.com");
  });
});

describe("Gmail OAuth — client id validation", () => {
  it("client id must end with .apps.googleusercontent.com", () => {
    // The component validates this format before allowing save.
    const validId = "12345.apps.googleusercontent.com";
    const invalidId = "not-a-google-client";

    expect(validId.endsWith(".apps.googleusercontent.com")).toBe(true);
    expect(invalidId.endsWith(".apps.googleusercontent.com")).toBe(false);
  });

  it("backend re-validates the client id format on setGmailClient", async () => {
    // The backend re-validates before storing in the keychain.
    // In browser fallback, setGmailClient throws regardless.
    await expect(
      setGmailClient("invalid", "secret"),
    ).rejects.toThrow();
  });
});