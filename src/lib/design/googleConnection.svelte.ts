// The Google connection state a screen holds — ONE implementation, shared by
// Email and Planner.
//
// Why it is shared: the two screens each had their own copy of "read the
// status" and "re-read it after a call failed", and the copies had DIVERGED on
// the case where nothing had been read yet — Planner adopted the fresh status,
// Email discarded it and left both banners dark. One of those was wrong, and
// nothing in the code said which. There is now one answer, in one place, with
// its own tests.
//
// Why the sequence tokens: every read (initial, manual re-check, post-failure
// refresh) CLAIMS a token before it starts, and only the newest claim may
// write. Without that, the re-check flow raced: clearing the state fires a
// status read, the retry that follows fails and re-records the state, and the
// refresh that would light the banner was thrown away because the effect's
// read had claimed a newer token — leaving the banner dark while the API was
// still switched off. Newest read wins, whichever resolves first.
//
// Why BOTH outcomes are observed: the backend records a verdict on failure AND
// clears one on success (a call that works proves that API is switched on).
// The screens only ever re-read after a failure, so the clearing half never
// reached the UI: once the user enabled the API in the console and the next
// call went through, the backend dropped the state and the banner went on
// rendering the stale one until a manual re-check. Re-reading after a SUCCESS
// too — through the same `refresh` — is the missing half.

import {
  gmailSetupStatus,
  googleClearApiNotEnabled,
  type DisabledApi,
  type GmailSetupStatus,
  type GoogleApiId,
} from "$lib/api/tauri";

/** The disabled-API ids in a status, in wire order. */
function disabledIds(status: GmailSetupStatus): string {
  return (status.api_not_enabled?.apis ?? []).map((api) => api.id).join(",");
}

/** The console links in a status, positionally, so a link changing on an API
 *  that was already listed still counts as a change (the banner renders it). */
function disabledLinks(status: GmailSetupStatus): string {
  return (status.api_not_enabled?.apis ?? []).map((api) => api.console_url ?? "").join(",");
}

/** Has the CONNECTION state (as opposed to the mail or planner data) changed?
 *  Both recoverable Google failures count, because both drive a banner. */
export function connectionStateChanged(a: GmailSetupStatus, b: GmailSetupStatus): boolean {
  return (
    a.needs_reconnect !== b.needs_reconnect ||
    (a.api_not_enabled == null) !== (b.api_not_enabled == null) ||
    disabledIds(a) !== disabledIds(b) ||
    disabledLinks(a) !== disabledLinks(b)
  );
}

/** Should a status read after a call replace what the screen holds?
 *
 *  Yes when nothing has been read yet — a screen with no status shows no
 *  banner at all, so keeping "nothing" over a real answer is the one outcome
 *  that helps nobody. Otherwise only when the connection state actually
 *  changed, so a re-read can't churn the effects that depend on it. */
export function shouldAdopt(fresh: GmailSetupStatus, current: GmailSetupStatus | null): boolean {
  return current == null || connectionStateChanged(fresh, current);
}

/** One disabled API as the banner renders it: a name, and the link Google gave
 *  for THAT API (or none). */
export type DisabledApiLink = { label: string; console_url: string | null };

/** What THIS screen should render of a profile-wide disabled-API state, or
 *  `null` when none of it is its business.
 *
 *  The state is per-profile — a Calendar failure the Planner (or an agent
 *  tool) recorded is visible to Email too — but the banner's "I've enabled it
 *  — check again" is per-screen: it clears and re-tests only the APIs the
 *  screen actually calls. Rendering the whole profile's state therefore
 *  produced a banner whose button could not fix what it named: Email would
 *  announce "Google Calendar isn't switched on" and offer a re-check that
 *  clears and retries Gmail.
 *
 *  The entries stay SEPARATE all the way to the banner. Flattening them to one
 *  link ("the first entry that has one") re-created the same mismatch inside a
 *  single screen: Planner with both Calendar and Tasks off named two APIs and
 *  offered only Calendar's activation page, so the user enabled one, retried,
 *  and got the identical banner back with no way to reach the other page. */
export function disabledFor(
  status: GmailSetupStatus | null,
  apis: GoogleApiId[],
): { apis: DisabledApiLink[] } | null {
  const mine: DisabledApi[] = (status?.api_not_enabled?.apis ?? []).filter((api) =>
    apis.includes(api.id),
  );
  if (mine.length === 0) return null;
  return {
    apis: mine.map((api) => ({ label: api.label, console_url: api.console_url })),
  };
}

export class GoogleConnection {
  /** The last connection state read, or null when none has been. */
  status = $state<GmailSetupStatus | null>(null);
  /** Why the last full read failed. Email surfaces this; Planner deliberately
   *  doesn't (its loads carry their own errors) — but both leave the banners
   *  dark rather than claim a state they could not read. */
  error = $state<string | null>(null);
  /** A clear-and-re-check is in flight (the banner disables its button). */
  checking = $state(false);

  #seq = 0;

  /** Claim the newest read. Any read that claimed earlier may no longer
   *  write, whenever it happens to resolve. */
  #claim(): number {
    return ++this.#seq;
  }

  /** Drop everything: the connection is per-profile, so a profile switch
   *  invalidates it (and anything already in flight for the old one). */
  reset(): void {
    this.#claim();
    this.status = null;
    this.error = null;
  }

  /** A full read — the initial check and every manual re-check. */
  async read(profile: string): Promise<void> {
    const token = this.#claim();
    this.error = null;
    try {
      const fresh = await gmailSetupStatus(profile);
      if (token === this.#seq) this.status = fresh;
    } catch (err) {
      if (token === this.#seq) {
        this.error = String(err);
        this.status = null;
      }
    }
  }

  /** Re-read after a call DECIDED something — either way.
   *
   *  A failure records the state backend-side, so without a re-read a failed
   *  read, send, or create would leave both banners dark and show only the raw
   *  error. A success records too: it is proof the API it reached is switched
   *  on, and the backend drops that API's disabled state on the spot, so
   *  without a re-read the banner kept rendering a state the backend had
   *  already thrown away until the user pressed "check again" by hand.
   *
   *  There is deliberately no cheap skip for "this screen holds no
   *  disabled-API claim, so a success can only re-confirm what we hold". That
   *  reasoning is wrong: the read is of the WHOLE profile status, so it also
   *  REVEALS a state some other path recorded since this screen last looked —
   *  precisely the agent-tool case this work added. An agent's Tasks call
   *  records "Tasks is off" while the Planner sits open holding nothing; the
   *  user's next Planner action is a Calendar one, it succeeds, and a skip
   *  here would leave the Tasks banner dark. The price of not skipping is one
   *  local status read per user-initiated action.
   *
   *  `shouldAdopt` still gates the WRITE, so a re-read that finds the
   *  connection unchanged cannot churn the effects watching it. */
  async refresh(profile: string): Promise<void> {
    const token = this.#claim();
    try {
      const fresh = await gmailSetupStatus(profile);
      if (token === this.#seq && shouldAdopt(fresh, this.status)) this.status = fresh;
    } catch {
      // Keep whatever error the caller already surfaced — a failed re-read is
      // not a new fact about the connection. (Nor, on the success path, is a
      // status read that fails any evidence the call that succeeded didn't.)
    }
  }

  /** "I've enabled it — check again": forget the disabled state for the APIs
   *  this screen can re-test. Returns an error message when the CLEAR itself
   *  failed (a local IPC problem, not a Google verdict), else null.
   *
   *  The caller then retries the calls that decide, and re-reads. Nothing is
   *  assumed fixed: a still-disabled API re-records itself. */
  async clearDisabled(profile: string, apis: GoogleApiId[]): Promise<string | null> {
    if (this.checking) return null;
    this.checking = true;
    try {
      await googleClearApiNotEnabled(profile, apis);
      return null;
    } catch (err) {
      return String(err);
    } finally {
      this.checking = false;
    }
  }
}
