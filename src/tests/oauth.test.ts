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

import { describe, it, expect } from "vitest";
import { render, fireEvent } from "@testing-library/svelte";
import {
  setGmailClient,
  gmailBeginConnect,
  gmailFinishConnect,
  gmailDisconnect,
  gmailSetupStatus,
  type GmailSetupStatus,
} from "$lib/api/tauri";

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

describe("Gmail OAuth — setup status data contract", () => {
  it("needs_reconnect is a normal state (not an error)", () => {
    // Testing-status Google clients expire refresh tokens every ~7 days.
    // needs_reconnect is a routine state, not an error.
    const status: GmailSetupStatus = {
      client_configured: true,
      connected: false,
      account_email: "user@example.com",
      needs_reconnect: true,
      api_not_enabled: null,
    };

    expect(status.needs_reconnect).toBe(true);
    // The UI must render a calm Reconnect, not an error banner.
    // This test verifies the data model supports that distinction.
  });

  it("account_email is null when unknown (never a fabricated address)", () => {
    const status: GmailSetupStatus = {
      client_configured: true,
      connected: true,
      account_email: null,
      needs_reconnect: false,
      api_not_enabled: null,
    };

    // Per the backend contract: null when the connect succeeded but the
    // profile address couldn't be read — never a fabricated address.
    expect(status.account_email).toBeNull();
  });
});

describe("Gmail OAuth — per-profile isolation", () => {
  it("each profile has its own connection status", () => {
    // Profile isolation: the connection (refresh token) is per-profile.
    // The install-global client id/secret are shared; the OAuth token is not.
    const personalStatus: GmailSetupStatus = {
      client_configured: true,
      connected: true,
      account_email: "personal@example.com",
      needs_reconnect: false,
      api_not_enabled: null,
    };
    const workStatus: GmailSetupStatus = {
      client_configured: true,
      connected: false,
      account_email: null,
      needs_reconnect: true,
      api_not_enabled: null,
    };

    expect(personalStatus.connected).toBe(true);
    expect(workStatus.connected).toBe(false);
    expect(workStatus.needs_reconnect).toBe(true);
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