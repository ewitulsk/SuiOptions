// Light/dark mode store — same shape as frontend/src/theme.ts, own
// storage key.

import { useSyncExternalStore } from "react";

export type ThemeMode = "light" | "dark";

const STORAGE_KEY = "desk-dash-theme-mode";

function readStored(): ThemeMode {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === "light" || v === "dark") return v;
  } catch {
    /* ignore */
  }
  return "dark";
}

let currentMode: ThemeMode = readStored();
const listeners = new Set<() => void>();

function applyToDom(mode: ThemeMode) {
  if (typeof document === "undefined") return;
  document.documentElement.setAttribute("data-mode", mode);
}

applyToDom(currentMode);

export function getMode(): ThemeMode {
  return currentMode;
}

export function setMode(next: ThemeMode) {
  if (next === currentMode) return;
  currentMode = next;
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {
    /* ignore */
  }
  applyToDom(next);
  listeners.forEach((l) => l());
}

export function toggleMode() {
  setMode(currentMode === "dark" ? "light" : "dark");
}

export function useThemeMode(): ThemeMode {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    getMode,
    getMode,
  );
}
