// App-level transient notifications ("toasts") — the UI consumers for the
// backend's non-silent signals that don't belong to a specific message bubble:
// `stream:local_reroute` (C5 — a turn was force-moved to a local provider) and
// `stream:budget_warning` (C1 — an attended turn is over its spend cap).
//
// Deliberately tiny: a writable list + push/dismiss with an auto-expiry. The
// design language maps `kind` to the routing palette (color = routing meaning,
// chrome stays grayscale): `local` = the green "stayed on your machine" tone,
// `warn` = the amber budget/caution tone.

import { writable } from "svelte/store";

export type ToastKind = "local" | "warn";

export interface Toast {
  id: number;
  kind: ToastKind;
  title: string;
  /** Optional second line; rendered as escaped text, never HTML. */
  body?: string;
}

/** How long a toast stays up before auto-dismissing (ms). */
const TOAST_TTL_MS = 8000;

let nextId = 1;

export const toasts = writable<Toast[]>([]);

/** Push a toast; it auto-dismisses after `TOAST_TTL_MS`. */
export function pushToast(kind: ToastKind, title: string, body?: string): void {
  const id = nextId++;
  toasts.update((list) => [...list, { id, kind, title, body }]);
  setTimeout(() => dismissToast(id), TOAST_TTL_MS);
}

export function dismissToast(id: number): void {
  toasts.update((list) => list.filter((t) => t.id !== id));
}
