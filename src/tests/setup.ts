// Test setup — runs before every test file.
// Sets up jsdom polyfills, localStorage mock, and Tauri-API stubs.

import "@testing-library/jest-dom/vitest";
import { beforeEach } from "vitest";

// Polyfill crypto.randomUUID for jsdom (Node 19+ has it, jsdom may not expose it).
if (typeof crypto !== "undefined" && !crypto.randomUUID) {
  Object.defineProperty(crypto, "randomUUID", {
    value: () =>
      "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (c) => {
        const r = (Math.random() * 16) | 0;
        return (c === "x" ? r : (r & 0x3) | 0x8).toString(16);
      }),
    writable: true,
  });
}

// Stub window.__TAURI_INTERNALS__ so tauri.ts falls back to browser mode
// in all tests. Individual test files can override this per-test.
if (typeof window !== "undefined") {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined;
}

// Clear localStorage between tests
beforeEach(() => {
  localStorage.clear();
});