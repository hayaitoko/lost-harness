// Screen navigation store — the Svelte equivalent of the React lib's nav.tsx
// (hash-router + context). A plain runes module: components import `nav` and
// call `nav.go(id)` / read `nav.current`. Screens are the top-level surfaces
// (chat, email, files, whiteboard, scheduled jobs, settings, …).

import { SCREEN_IDS, type ScreenId } from "./types";

function readHash(fallback: ScreenId): ScreenId {
  if (typeof window === "undefined") return fallback;
  const raw = window.location.hash.replace(/^#\/?/, "");
  return (SCREEN_IDS as string[]).includes(raw) ? (raw as ScreenId) : fallback;
}

class Nav {
  current = $state<ScreenId>(readHash("main"));

  constructor() {
    if (typeof window !== "undefined") {
      window.addEventListener("hashchange", () => {
        this.current = readHash(this.current);
      });
    }
  }

  /** Navigate to a screen; also updates the URL hash so deep-links work. */
  go(id: ScreenId) {
    this.current = id;
    if (typeof window !== "undefined") window.location.hash = "#/" + id;
  }
}

export const nav = new Nav();
