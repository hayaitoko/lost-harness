// Shared design-system types, ported from ~/Desktop/lost-harness-ui/src/types.ts.
// These mirror the Rust core's routing vocabulary (agent::gate::Binding /
// GateDecision) so the UI and backend speak the same language.

/** Where a message/turn actually went. The product's core meaning-color signal. */
export type Route = "local" | "cloud" | "blocked";

/** A conversation's binding — the user's routing *intent*. */
export type Binding = "auto" | "public" | "private";

/** The top-level surfaces reachable from the left-rail section nav + chat/settings.
 *  Nav honesty (2026-07-24 bridge campaign): only LIVE screens are listed.
 *  The Email / Whiteboard / Editor / Onboarding / EmptyState mockups were
 *  removed from the map — their backends don't exist yet (email/calendar is
 *  deferred to its own round by the 2026-07-22 scope decision), and a nav
 *  entry that opens dead UI misleads. Re-add each id here when its backend
 *  lands. */
export type ScreenId = "main" | "files" | "scheduled-jobs" | "settings";

export const SCREEN_IDS: ScreenId[] = [
  "main",
  "files",
  "scheduled-jobs",
  "settings",
];
