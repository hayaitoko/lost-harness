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
import { tick } from "svelte";
import GoogleApiDisabledBanner from "$lib/design/components/GoogleApiDisabledBanner.svelte";
import Email from "$lib/design/screens/Email.svelte";
import Planner from "$lib/design/screens/Planner.svelte";
import {
  connectionStateChanged,
  disabledFor,
  shouldAdopt,
} from "$lib/design/googleConnection.svelte";
import type { GmailSetupStatus, GoogleApiDisabled, GoogleApiId } from "$lib/api/tauri";

const CONSOLE_URL =
  "https://console.developers.google.com/apis/api/tasks.googleapis.com/overview?project=42";

/** Let every pending promise chain and the resulting render finish. Used where
 *  the point of the test is the state AFTER a late write lands — `waitFor`
 *  would happily pass on the state before it. */
async function settle(): Promise<void> {
  for (let i = 0; i < 5; i++) await new Promise((resolve) => setTimeout(resolve, 0));
  await tick();
}

const LABEL: Record<GoogleApiId, string> = {
  gmail: "Gmail",
  calendar: "Google Calendar",
  tasks: "Google Tasks",
};

/** The disabled-API state as the backend sends it: ONE ENTRY PER API, each
 *  with its own wire id, its own label and its own console link. */
function disabledState(
  apis: GoogleApiId[] = ["tasks"],
  console_url: string | null = CONSOLE_URL,
): GoogleApiDisabled {
  return { apis: apis.map((id) => ({ id, label: LABEL[id], console_url })) };
}

// ── the banner component on its own ─────────────────────────────────────────

describe("GoogleApiDisabledBanner", () => {
  afterEach(() => cleanup());

  it("links to the page Google pointed to, and never offers a reconnect", () => {
    const { getByTestId, container } = render(GoogleApiDisabledBanner, {
      consoleUrl: CONSOLE_URL,
      apis: ["Google Tasks"],
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

  it("names the APIs the backend recorded, and stays vague only when it must", async () => {
    const oncheckagain = () => {};
    const { container, rerender } = render(GoogleApiDisabledBanner, {
      consoleUrl: null,
      apis: ["Google Tasks"],
      oncheckagain,
    });
    // The backend knows exactly which API answered SERVICE_DISABLED, so making
    // the user guess between three would be throwing information away.
    expect(container.textContent).toMatch(/Google Tasks isn't switched on/i);

    await rerender({ consoleUrl: null, apis: ["Gmail", "Google Tasks"], oncheckagain });
    expect(container.textContent).toMatch(/Gmail and Google Tasks/);

    // …and only when nothing was recorded does it fall back to the vaguer copy.
    await rerender({ consoleUrl: null, apis: [], oncheckagain });
    expect(container.textContent).toMatch(/A Google API isn't switched on/i);
  });

  it("falls back to the API library, and says so, when Google gave no link", () => {
    const { getByTestId, container } = render(GoogleApiDisabledBanner, {
      consoleUrl: null,
      apis: ["Google Tasks"],
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
      apis: ["Google Tasks"],
      oncheckagain,
    });

    await fireEvent.click(getByText(/I've enabled it/));
    expect(oncheckagain).toHaveBeenCalledTimes(1);

    // The remedy happens outside the app, so this button is one of the two
    // ways out (a successful call is the other) — it must not be
    // double-fireable.
    await rerender({ consoleUrl: CONSOLE_URL, apis: ["Google Tasks"], oncheckagain, checking: true });
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

  /// Each screen with the API it OWNS — the one its "check again" button
  /// actually clears and re-tests — and the one that belongs to the other
  /// screen entirely.
  const SCREENS = [
    { name: "Email", Screen: Email, own: "gmail", label: "Gmail", theirs: "calendar" },
    {
      name: "Planner",
      Screen: Planner,
      own: "tasks",
      label: "Google Tasks",
      theirs: "gmail",
    },
  ] as const;

  for (const { name, Screen, own, theirs } of SCREENS) {
    it(`${name}: a disabled API shows the console banner and NO reconnect`, async () => {
      routeInvoke(status({ api_not_enabled: disabledState([own]) }));
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

    /// The half that was missing. The backend records on BOTH outcomes: a
    /// call that completes is proof its API is switched on, and clears the
    /// disabled state on the spot. The screens only ever re-read after a
    /// FAILURE, so the clearing half never reached the UI — the user enabled
    /// the API in the console, the very next call went through, and the banner
    /// went on rendering a state the backend had already thrown away until
    /// they pressed "check again" by hand.
    it(`${name}: a call that succeeds takes the banner down, with no manual re-check`, async () => {
      let apiOn = false; // the switch in the user's Google Cloud console
      let recorded = true; // what the backend currently holds
      tauri.invoke.mockImplementation(async (cmd: string) => {
        switch (cmd) {
          case "gmail_setup_status":
            return status(recorded ? { api_not_enabled: disabledState([own]) } : {});
          case "list_email":
          case "list_calendar_events":
          case "list_google_tasks":
            if (!apiOn) {
              recorded = true; // observe_failure
              throw new Error("Google API HTTP 403: switched off");
            }
            recorded = false; // observe_success — the call proves the API is on
            return [];
          default:
            return null;
        }
      });

      const { getByRole, queryByTestId } = render(Screen);
      await waitFor(() =>
        expect(queryByTestId("google-api-disabled-banner")).not.toBeNull(),
      );

      // The user switches it on in the console and just uses the app again.
      // No "I've enabled it — check again" press anywhere in this test.
      apiOn = true;
      await fireEvent.click(getByRole("button", { name: "Refresh" }));
      await settle();

      expect(queryByTestId("google-api-disabled-banner")).toBeNull();
      expect(tauri.invoke).not.toHaveBeenCalledWith(
        "google_clear_api_not_enabled",
        expect.anything(),
      );
    });

    /// The banner is drawn from a PROFILE-wide state, but the button under it
    /// is screen-scoped: it clears and re-tests only this screen's APIs. A
    /// banner naming an API this screen never calls is a button that cannot
    /// fix what it names — Email announcing "Google Calendar isn't switched
    /// on" above a re-check that clears and retries Gmail.
    it(`${name}: renders no banner for an API only the other screen can re-test`, async () => {
      routeInvoke(status({ api_not_enabled: disabledState([theirs]) }));
      const { queryByTestId } = render(Screen);

      await waitFor(() =>
        expect(tauri.invoke).toHaveBeenCalledWith(
          "gmail_setup_status",
          expect.anything(),
        ),
      );
      await settle();
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
            disabled ? { api_not_enabled: disabledState() } : {},
          );
        case "list_calendar_events":
        case "list_google_tasks":
          return [];
        case "create_google_task":
          // The failing call is what records the state backend-side.
          disabled = true;
          throw new Error(
            "Google Tasks API HTTP 403: this Google API is switched off in your " +
              "Google Cloud project.",
          );
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

  /// The re-check race. Pressing "I've enabled it — check again" starts TWO
  /// things: a status read (the state was just cleared, so it reads clean) and
  /// a retry (which fails, re-records the state, and re-reads). If the stale
  /// clean read is allowed to land last, the banner goes dark while the API is
  /// still switched off — the app asserting something false about Google,
  /// which is the one thing this whole feature exists to stop.
  ///
  /// Driven in the WORST order on purpose: both reads are held, then released
  /// stale-LAST, and the assertion happens after they have both settled (a
  /// `waitFor` here would pass on the state as it is before the stale write).
  it("Planner: a failed re-check leaves the banner lit, whatever order the reads land in", async () => {
    let disabled = true;
    let holdReads = false;
    const held: Array<{ sawDisabled: boolean; release: () => void }> = [];

    tauri.invoke.mockImplementation(async (cmd: string) => {
      switch (cmd) {
        case "gmail_setup_status": {
          // Captured NOW, resolved later — that is what makes a read stale.
          const sawDisabled = disabled;
          const snapshot = status(
            sawDisabled ? { api_not_enabled: disabledState() } : {},
          );
          if (!holdReads) return snapshot;
          return new Promise((resolve) =>
            held.push({ sawDisabled, release: () => resolve(snapshot) }),
          );
        }
        case "google_clear_api_not_enabled":
          disabled = false;
          return null;
        case "list_google_tasks":
          // The retry: Tasks is STILL off, so the call re-records the state.
          disabled = true;
          throw new Error("Google Tasks API HTTP 403: switched off");
        case "list_calendar_events":
          return [];
        default:
          return null;
      }
    });

    const { getByText, queryByTestId } = render(Planner);
    await waitFor(() => expect(queryByTestId("google-api-disabled-banner")).not.toBeNull());

    holdReads = true;
    await fireEvent.click(getByText(/I've enabled it/));
    // Both reads are now in flight: the post-clear one (clean) and the
    // post-failure one (disabled again).
    await waitFor(() => expect(held.length).toBe(2));
    const stale = held.find((read) => !read.sawDisabled);
    const fresh = held.find((read) => read.sawDisabled);
    expect(stale && fresh).toBeTruthy();

    fresh!.release();
    stale!.release();
    await settle();

    expect(queryByTestId("google-api-disabled-banner")).not.toBeNull();
  });

  /// A screen may only clear what it can re-test. Email clearing Calendar or
  /// Tasks would blank a banner nothing on that screen will ever retry, and
  /// the user would be told the problem was gone.
  it.each([
    ["Email", Email, ["gmail"], "gmail"],
    ["Planner", Planner, ["calendar", "tasks"], "tasks"],
  ] as const)("%s: the re-check clears only the APIs it can re-test", async (_name, Screen, apis, own) => {
    routeInvoke(status({ api_not_enabled: disabledState([own]) }));
    const { getByText } = render(Screen);
    await waitFor(() => getByText(/I've enabled it/));

    await fireEvent.click(getByText(/I've enabled it/));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("google_clear_api_not_enabled", {
        args: { profile: expect.any(String), apis },
      }),
    );
  });

  /// Two APIs off at once, one per screen. Each screen must name ITS OWN and
  /// link where Google pointed FOR THAT ONE. The flattened state this replaces
  /// carried a single label list plus "the first link in API order", so Email
  /// would name Gmail while linking to the page Google gave for Calendar.
  it("with two APIs off, each screen names its own and links to its own page", async () => {
    const GMAIL_URL =
      "https://console.developers.google.com/apis/api/gmail.googleapis.com/overview?project=42";
    const both: GoogleApiDisabled = {
      apis: [
        { id: "gmail", label: "Gmail", console_url: GMAIL_URL },
        { id: "calendar", label: "Google Calendar", console_url: CONSOLE_URL },
      ],
    };
    routeInvoke(status({ api_not_enabled: both }));

    const email = render(Email);
    const emailBanner = await waitFor(() =>
      email.getByTestId("google-api-disabled-banner"),
    );
    expect(emailBanner.textContent).toMatch(/Gmail isn't switched on/);
    expect(emailBanner.textContent).not.toMatch(/Calendar/);
    expect(email.getByTestId("google-api-console-link").getAttribute("href")).toBe(
      GMAIL_URL,
    );
    cleanup();

    const planner = render(Planner);
    const plannerBanner = await waitFor(() =>
      planner.getByTestId("google-api-disabled-banner"),
    );
    expect(plannerBanner.textContent).toMatch(/Google Calendar isn't switched on/);
    expect(plannerBanner.textContent).not.toMatch(/Gmail/);
    expect(planner.getByTestId("google-api-console-link").getAttribute("href")).toBe(
      CONSOLE_URL,
    );
  });

  /// Planner's specific gap: it read no connection state, so it printed the
  /// raw backend error under fixed prose that told the user to reconnect —
  /// advice that is simply wrong for a disabled API.
  it("Planner: stops advising a reconnect once a banner names the real problem", async () => {
    routeInvoke(status({ api_not_enabled: disabledState() }));
    const { getByTestId, queryByText } = render(Planner);

    await waitFor(() => getByTestId("google-api-disabled-banner"));
    expect(
      queryByText(/connect or reconnect Google from Email/i),
    ).toBeNull();
  });
});

// ── the shared connection-state rules ───────────────────────────────────────

describe("the shared connection-state decision", () => {
  const base: GmailSetupStatus = {
    client_configured: true,
    connected: true,
    account_email: "ada@example.com",
    needs_reconnect: false,
    api_not_enabled: null,
  };

  /// The divergence this replaces: Planner adopted a fresh status when it held
  /// none, Email discarded it. Email's copy was the wrong one — a screen that
  /// holds no status shows NO banner, so refusing the only answer available
  /// keeps both banners dark for exactly the failure that just happened.
  it("adopts a fresh status when nothing has been read yet", () => {
    expect(shouldAdopt(base, null)).toBe(true);
  });

  /// …but not when nothing about the CONNECTION changed, or every re-read
  /// would re-trigger the effects that watch it.
  it("keeps what it has when the connection state is unchanged", () => {
    expect(shouldAdopt({ ...base, account_email: "someone-else@example.com" }, base)).toBe(
      false,
    );
  });

  /// A screen may only render what its own "check again" button can act on.
  it("hands a screen only the entries it can re-test, with THAT API's link", () => {
    const twoOff: GmailSetupStatus = {
      ...base,
      api_not_enabled: {
        apis: [
          { id: "gmail", label: "Gmail", console_url: null },
          { id: "calendar", label: "Google Calendar", console_url: CONSOLE_URL },
        ],
      },
    };

    // Gmail carried no link of its own, and must not borrow Calendar's — the
    // banner then points at the API library in prose instead.
    expect(disabledFor(twoOff, ["gmail"])).toEqual({
      console_url: null,
      apis: ["Gmail"],
    });
    expect(disabledFor(twoOff, ["calendar", "tasks"])).toEqual({
      console_url: CONSOLE_URL,
      apis: ["Google Calendar"],
    });
    // Nothing of this screen's is off → no banner at all, rather than one
    // whose button cannot fix what it names.
    expect(disabledFor(twoOff, ["tasks"])).toBeNull();
    expect(disabledFor(base, ["gmail"])).toBeNull();
    expect(disabledFor(null, ["gmail"])).toBeNull();
  });

  it("notices each state a banner depends on", () => {
    expect(connectionStateChanged({ ...base, needs_reconnect: true }, base)).toBe(true);
    expect(
      connectionStateChanged({ ...base, api_not_enabled: disabledState() }, base),
    ).toBe(true);
    // …including WHICH apis are off and WHERE the link points, since both are
    // rendered.
    expect(
      connectionStateChanged(
        { ...base, api_not_enabled: disabledState(["gmail"]) },
        { ...base, api_not_enabled: disabledState(["tasks"]) },
      ),
    ).toBe(true);
    expect(
      connectionStateChanged(
        { ...base, api_not_enabled: disabledState(["gmail"], null) },
        { ...base, api_not_enabled: disabledState(["gmail"]) },
      ),
    ).toBe(true);
  });
});
