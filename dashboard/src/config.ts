// Environment targeting. The dashboard is read-only and every backend it
// reads is publicly routed with CORS `*`, so switching environments is a
// pure client-side concern: a localStorage-backed toggle that rewrites
// the service base URLs.

import { useSyncExternalStore } from "react";

export type DashEnv = "staging" | "prod" | "local";

const STORAGE_KEY = "desk-dash-env";
const DEFAULT_ENV = ((import.meta.env.VITE_DEFAULT_ENV as string | undefined) ??
  "staging") as DashEnv;

function readStored(): DashEnv {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === "staging" || v === "prod" || v === "local") return v;
  } catch {
    /* ignore */
  }
  return DEFAULT_ENV;
}

let currentEnv: DashEnv = readStored();
const listeners = new Set<() => void>();

export function getEnv(): DashEnv {
  return currentEnv;
}

export function setEnv(next: DashEnv) {
  if (next === currentEnv) return;
  currentEnv = next;
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {
    /* ignore */
  }
  listeners.forEach((l) => l());
}

export function useDashEnv(): DashEnv {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    getEnv,
    getEnv,
  );
}

export type ServiceUrls = {
  /** mm-bot ops server: /desk/state, /desk/history. */
  mmBot: string;
  /** api-service REST. */
  api: string;
  /** indexer GraphQL endpoint (full path). */
  indexerGraphql: string;
  /** hedge-signer: /frost/*, /bluefin/* relay. */
  hedgeSigner: string;
};

const PUBLIC_BASE = (import.meta.env.VITE_PUBLIC_BASE as string | undefined) ??
  "https://sui-options.com";

export function serviceUrls(env: DashEnv): ServiceUrls {
  if (env === "local") {
    return {
      mmBot: "http://127.0.0.1:8084",
      api: "http://127.0.0.1:9003",
      indexerGraphql: "http://127.0.0.1:9002/graphql",
      hedgeSigner: "http://127.0.0.1:9017",
    };
  }
  const base = `${PUBLIC_BASE}/${env}`;
  return {
    mmBot: `${base}/mm-bot`,
    api: `${base}/api`,
    indexerGraphql: `${base}/indexer/graphql`,
    hedgeSigner: `${base}/hedge-signer`,
  };
}

export function useServiceUrls(): ServiceUrls {
  return serviceUrls(useDashEnv());
}
