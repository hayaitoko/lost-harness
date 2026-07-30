// Shared design-system types, ported from ~/Desktop/lost-harness-ui/src/types.ts.
// These mirror the Rust core's routing vocabulary (agent::gate::Binding /
// GateDecision) so the UI and backend speak the same language.

/** Where a message/turn actually went. The product's core meaning-color signal.
 *
 * `"unknown"` is a first-class state, not a gap to be papered over: it is what
 * the UI shows when the backend did not stamp a trust zone on the turn (a row
 * older than the stamp). The alternative — quietly defaulting to `"local"` —
 * is a green badge on a turn that may well have gone to a public cloud
 * endpoint, which is worse than admitting we don't know. */
export type Route = "local" | "cloud" | "blocked" | "unknown";

/** A conversation's binding — the user's routing *intent*. */
export type Binding = "auto" | "public" | "private";

/** The top-level surfaces reachable from the left-rail section nav + chat/settings.
 *  Nav honesty (2026-07-24 bridge campaign): only LIVE screens are listed.
 *  Email is LIVE as of the 2026-07-24 email round (Gmail IPC + the guided
 *  per-user OAuth-client wizard). The Whiteboard / Editor / Onboarding /
 *  EmptyState mockups stay out of the map — their backends don't exist yet,
 *  and a nav entry that opens dead UI misleads. Re-add each id here when its
 *  backend lands. */
export type ScreenId = "main" | "email" | "planner" | "files" | "scheduled-jobs" | "settings";

export const SCREEN_IDS: ScreenId[] = [
  "main",
  "email",
  "planner",
  "files",
  "scheduled-jobs",
  "settings",
];
