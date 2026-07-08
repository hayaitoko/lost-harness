// Lost Harness — Settings (Svelte stores).
//
// App-level preferences. M1 stub: in-memory + localStorage only. The real
// impl will route through the Rust config store (sled/redb) so settings
// follow the user across installs.

import { writable, get, type Writable } from "svelte/store";

export type Theme = "dark" | "light" | "system";

const STORAGE_KEY = "lh.settings.v1";

interface PersistedSettings {
  theme: Theme;
  sendOnEnter: boolean;
}

const DEFAULTS: PersistedSettings = {
  theme: "dark",
  sendOnEnter: true,
};

function loadFromStorage(): PersistedSettings {
  if (typeof localStorage === "undefined") return { ...DEFAULTS };
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...DEFAULTS };
    const parsed = JSON.parse(raw) as Partial<PersistedSettings>;
    return { ...DEFAULTS, ...parsed };
  } catch {
    return { ...DEFAULTS };
  }
}

function persist(s: PersistedSettings): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(s));
  } catch {
    // localStorage may be unavailable (private mode, quota); non-fatal.
  }
}

const initial = loadFromStorage();

/** Active theme. "system" follows the OS preference via a media query. */
export const theme: Writable<Theme> = writable<Theme>(initial.theme);

/**
 * When true, pressing Enter in the chat input sends the message and
 * Shift+Enter inserts a newline. When false, Enter inserts a newline and
 * the user must press a "Send" button (or Cmd/Ctrl+Enter).
 */
export const sendOnEnter: Writable<boolean> = writable<boolean>(initial.sendOnEnter);

// Single subscription that persists both keys whenever either changes.
function syncAndPersist(): void {
  persist({ theme: get(theme), sendOnEnter: get(sendOnEnter) });
}
theme.subscribe(syncAndPersist);
sendOnEnter.subscribe(syncAndPersist);

/** Apply the theme by setting `data-theme` on <html>. Safe to call repeatedly. */
export function applyTheme(value: Theme): void {
  if (typeof document === "undefined") return;
  const root = document.documentElement;
  if (value === "system") {
    const prefersDark =
      window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? true;
    root.setAttribute("data-theme", prefersDark ? "dark" : "light");
  } else {
    root.setAttribute("data-theme", value);
  }
}

// Keep the DOM in sync when the store changes.
theme.subscribe(applyTheme);
