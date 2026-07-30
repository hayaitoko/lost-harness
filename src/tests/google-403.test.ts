/// <reference types="vitest" />

/**
 * Google connector 403 recovery — the TWO-BANNER contract.
 *
 * A Google API call can fail two recoverable ways, and the whole point of this
 * work is that they must never share a banner:
 *
 * - the stored grant is missing a scope → reconnecting re-consents all four
 *   scopes, so the calm Reconnect strip is the right answer;
 * - the API is switched OFF in the user's own Google Cloud project →
 *   reconnecting re-consents the same scopes against the same project and
 *   fails identically. Offering Reconnect here is an infinite loop with no
 *   exit, so this state gets its own banner: a console link, no Reconnect.
 *
 * Both Email and Planner must show both. Planner previously read NO connection
 * state at all — it dumped the raw backend error under fixed prose telling the
 * user to reconnect, whether or not that could possibly help.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, fireEvent, cleanup, waitFor } from "@testing-library/svelte";
import GoogleApiDisabledBanner from "$lib/design/components/GoogleApiDisabledBanner.svelte";
import Email from "$lib/design/screens/Email.svelte";
import Planner from "$lib/design/screens/Planner.svelte";
import type { GmailSetupStatus } from "$lib/api/tauri";

const CONSOLE_URL =
  "https://console.developers.google.com/apis/api/tasks.googleapis.com/overview?project=42";

// ── the banner component on its own ─────────────────────────────────────────

describe("GoogleApiDisabledBanner", () => {
  afterEach(() => cleanup());

  it("links to the page Google pointed to, and never offers a reconnect", () => {
    const { getByTestId, container } = render(GoogleApiDisabledBanner, {
      consoleUrl: CONSOLE_URL,
      oncheckagain: () => {},
    });

    const link = getByTestId("google-api-console-link");
    expect(link.getAttribute("href")).toBe(CONSOLE_URL);
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toContain("noopener");

    // The load-bearing absence: a Reconnect CONTROL here would send the user
    // through the OAuth dance to fail identically, forever. (The prose does
    // say the word — it has to explain why reconnecting is not the answer.)
    const actions = Array.from(container.querySelectorAll("button, a"));
    expect(
      actions.filter((el) => /^\s*reconnect/i.test(el.textContent ?? "")),
    ).toHaveLength(0);
    // …and the copy has to say why, or the missing button reads as an omission.
    expect(container.textContent).toMatch(/switch(ed)? off in your Google Cloud project/i);
    expect(container.textContent).toMatch(/Reconnecting won't help/i);
  });

  it("falls back to the API library, and says so, when Google gave no link", () => {
    const { getByTestId, container } = render(GoogleApiDisabledBanner, {
      consoleUrl: null,
      oncheckagain: () => {},
    });

    // A known-good page of ours — never a URL guessed out of the error text.
    expect(getByTestId("google-api-console-link").getAttribute("href")).toBe(
      "https://console.cloud.google.com/apis/library",
    );
    expect(container.textContent).toMatch(/didn't include a direct link/i);
  });

  it("offers an explicit re-check, disabled while one is in flight", async () => {
    const oncheckagain = vi.fn();
    const { getByText, rerender } = render(GoogleApiDisabledBanner, {
      consoleUrl: CONSOLE_URL,
      oncheckagain,
    });

    await fireEvent.click(getByText(/I've enabled it/));
    expect(oncheckagain).toHaveBeenCalledTimes(1);

    // The remedy happens outside the app, so this button is the ONLY thing
    // that clears the sticky state — it must not be double-fireable.
    await rerender({ consoleUrl: CONSOLE_URL, oncheckagain, checking: true });
    expect(getByText("Checking…")).toBeDisabled();
  });
});

// ── the screens ─────────────────────────────────────────────────────────────

const tauri = vi.hoisted(() => ({
  invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async () => () => {},
}));

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

/** Route every command the two screens touch; `gmail_setup_status` is the one
 *  under test, everything else returns something empty and harmless. */
function routeInvoke(setupStatus: GmailSetupStatus) {
  tauri.invoke.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "gmail_setup_status":
        return setupStatus;
      case "list_email":
      case "list_calendar_events":
      case "list_google_tasks":
        return [];
      default:
        return null;
    }
  });
}

describe("Email + Planner — the two connection banners", () => {
  beforeEach(() => {
    tauri.invoke.mockReset();
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  });
  afterEach(() => {
    cleanup();
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined;
  });

  for (const [name, Screen] of [
    ["Email", Email],
    ["Planner", Planner],
  ] as const) {
    it(`${name}: a disabled API shows the console banner and NO reconnect`, async () => {
      routeInvoke(status({ api_not_enabled: { console_url: CONSOLE_URL } }));
      const { getByTestId, queryByText } = render(Screen);

      const banner = await waitFor(() => getByTestId("google-api-disabled-banner"));
      expect(banner.textContent).toMatch(/isn't switched on/i);
      expect(getByTestId("google-api-console-link").getAttribute("href")).toBe(
        CONSOLE_URL,
      );
      // The separation, asserted from the user's side: no reconnect affordance.
      expect(queryByText("Reconnect")).toBeNull();
      expect(queryByText("Reconnect in Email")).toBeNull();
    });

    it(`${name}: a scope-short grant shows reconnect and NOT the console banner`, async () => {
      routeInvoke(status({ needs_reconnect: true }));
      const { queryByTestId, getByText } = render(Screen);

      await waitFor(() =>
        expect(
          getByText(name === "Email" ? "Reconnect" : "Reconnect in Email"),
        ).toBeInTheDocument(),
      );
      expect(queryByTestId("google-api-disabled-banner")).toBeNull();
    });

    it(`${name}: a healthy connection shows neither banner`, async () => {
      routeInvoke(status());
      const { queryByTestId, queryByText } = render(Screen);

      // Let the status read settle before asserting an absence.
      await waitFor(() =>
        expect(tauri.invoke).toHaveBeenCalledWith(
          "gmail_setup_status",
          expect.anything(),
        ),
      );
      expect(queryByTestId("google-api-disabled-banner")).toBeNull();
      expect(queryByText("Reconnect")).toBeNull();
      expect(queryByText("Reconnect in Email")).toBeNull();
    });
  }

  /// The banner must light off the call that ACTUALLY failed, not only off a
  /// list load. Here the initial status read is clean and only a create fails
  /// — exactly the case that used to leave both banners dark and show nothing
  /// but raw `Google API HTTP 403` text.
  it("Planner: a failed create lights the banner, not just a failed list", async () => {
    let disabled = false;
    tauri.invoke.mockImplementation(async (cmd: string) => {
      switch (cmd) {
        case "gmail_setup_status":
          return status(
            disabled ? { api_not_enabled: { console_url: CONSOLE_URL } } : {},
          );
        case "list_calendar_events":
        case "list_google_tasks":
          return [];
        case "create_google_task":
          // The failing call is what records the state backend-side.
          disabled = true;
          throw new Error("[google:api_not_enabled] Google API HTTP 403");
        default:
          return null;
      }
    });

    const { getByPlaceholderText, getByText, queryByTestId } = render(Planner);
    await waitFor(() => expect(queryByTestId("google-api-disabled-banner")).toBeNull());

    await fireEvent.input(getByPlaceholderText("New task"), {
      target: { value: "buy milk" },
    });
    await fireEvent.click(getByText("Add task"));

    await waitFor(() => expect(queryByTestId("google-api-disabled-banner")).not.toBeNull());
  });

  /// Planner's specific gap: it read no connection state, so it printed the
  /// raw backend error under fixed prose that told the user to reconnect —
  /// advice that is simply wrong for a disabled API.
  it("Planner: stops advising a reconnect once a banner names the real problem", async () => {
    routeInvoke(status({ api_not_enabled: { console_url: CONSOLE_URL } }));
    const { getByTestId, queryByText } = render(Planner);

    await waitFor(() => getByTestId("google-api-disabled-banner"));
    expect(
      queryByText(/connect or reconnect Google from Email/i),
    ).toBeNull();
  });
});
