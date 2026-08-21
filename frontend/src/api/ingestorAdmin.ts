// Event-ingestor admin client — the JWT-gated config plane of the
// go-backend `event-ingestor` service (tracked packages, event→points
// rules, ingestion status).
//
// Every endpoint (including reads) carries the admin JWT and delegates auth
// to auth-service, so the react-query hooks below are gated on a
// `getToken` callback from `useAdminAuth` — `null` before sign-in. Base URL
// from INGESTOR_URL (see config.ts).

import { useQuery } from "@tanstack/react-query";

import { INGESTOR_URL } from "../config";

const base = INGESTOR_URL.replace(/\/$/, "");

/** Thrown when the JWT is missing/expired/invalid (HTTP 401/403). */
export class AuthExpiredError extends Error {}

/** One struct field from the cached chain introspection. */
export type StructField = {
  name: string;
  /** Move type repr, e.g. `address` or `0x2::balance::Balance<...>`. */
  repr: string;
};

export type StructDto = {
  name: string;
  /** Uppercase Move abilities, e.g. `["COPY","DROP"]`. */
  abilities: string[];
  fields: StructField[];
};

export type ModuleDto = {
  name: string;
  structs: StructDto[];
};

export type TrackedPackageDto = {
  /** Normalized 0x-padded-64 package address. */
  package_address: string;
  label: string;
  modules: { package: string; modules: ModuleDto[] };
  introspected_at: string;
  created_by: string;
  created_at: string;
};

export type RecipientMode = "sender" | "field";
export type StartMode = "tip" | "timestamp";
export type BackfillState = "none" | "pending" | "running" | "done" | "exhausted";

export type RuleDto = {
  id: number;
  package_address: string;
  module_name: string;
  /** Canonical `0x<64>::module::Struct`. */
  event_type: string;
  label: string;
  points: number;
  recipient_mode: RecipientMode;
  recipient_field: string | null;
  start_mode: StartMode;
  start_at: string | null;
  backfill_state: BackfillState;
  enabled: boolean;
  created_by: string;
  created_at: string;
  updated_at: string;
};

/** Fields accepted by `POST /rules`. */
export type RuleInput = {
  package_address: string;
  module_name: string;
  event_type: string;
  label: string;
  points: number;
  recipient_mode: RecipientMode;
  recipient_field?: string;
  start_mode: StartMode;
  start_at?: string;
  enabled: boolean;
};

/** Fields accepted by `PATCH /rules/{id}`. */
export type RulePatch = {
  label?: string;
  points?: number;
  enabled?: boolean;
  recipient_mode?: RecipientMode;
  recipient_field?: string;
  start_mode?: StartMode;
  start_at?: string;
};

export type ModuleStatusDto = {
  package_address: string;
  module: string;
  cursor: string;
  cursor_updated_at: string;
  last_event_ms: number | null;
  lag_ms: number | null;
};

export type RuleStatusDto = {
  rule_id: number;
  backfill_state: BackfillState;
  delivered: number;
  last_delivery_at: string | null;
};

export type IngestorStatus = {
  modules: ModuleStatusDto[];
  rules: RuleStatusDto[];
};

/** Candidate-event heuristic: structs `event::emit` accepts — abilities
 * include COPY and DROP but not KEY. The admin confirms; GraphQL has no
 * "is event" marker. */
export function isCandidateEvent(s: StructDto): boolean {
  return (
    s.abilities.includes("COPY") &&
    s.abilities.includes("DROP") &&
    !s.abilities.includes("KEY")
  );
}

/** Canonical event type for a rule: the tracked package's 0x-padded-64
 * address (as stored) + module + struct. */
export function canonicalEventType(
  packageAddress: string,
  moduleName: string,
  structName: string,
): string {
  return `${packageAddress}::${moduleName}::${structName}`;
}

async function request(
  token: string,
  path: string,
  method: "GET" | "POST" | "PATCH" | "DELETE",
  body?: unknown,
): Promise<Response> {
  const res = await fetch(`${base}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      ...(body ? { "content-type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (res.status === 401 || res.status === 403) {
    throw new AuthExpiredError(await res.text());
  }
  if (!res.ok) {
    let msg = await res.text();
    try {
      const parsed = JSON.parse(msg) as { error?: string };
      if (parsed?.error) msg = parsed.error;
    } catch {
      // non-JSON error body; keep the raw text
    }
    throw new Error(`${method} ${path} → ${res.status}: ${msg}`);
  }
  return res;
}

export async function listPackages(token: string): Promise<TrackedPackageDto[]> {
  const res = await request(token, "/packages", "GET");
  return ((await res.json()) as { packages: TrackedPackageDto[] }).packages;
}

/** Track a package — the service introspects its modules synchronously;
 * 400 for unknown/eventless packages. */
export async function createPackage(
  token: string,
  input: { package_address: string; label?: string },
): Promise<TrackedPackageDto> {
  const res = await request(token, "/packages", "POST", input);
  return ((await res.json()) as { package: TrackedPackageDto }).package;
}

export async function deletePackage(token: string, packageAddress: string): Promise<void> {
  await request(token, `/packages/${encodeURIComponent(packageAddress)}`, "DELETE");
}

export async function listRules(token: string): Promise<RuleDto[]> {
  const res = await request(token, "/rules", "GET");
  return ((await res.json()) as { rules: RuleDto[] }).rules;
}

export async function createRule(token: string, input: RuleInput): Promise<RuleDto> {
  const res = await request(token, "/rules", "POST", input);
  return ((await res.json()) as { rule: RuleDto }).rule;
}

export async function patchRule(
  token: string,
  id: number,
  patch: RulePatch,
): Promise<RuleDto> {
  const res = await request(token, `/rules/${id}`, "PATCH", patch);
  return ((await res.json()) as { rule: RuleDto }).rule;
}

export async function deleteRule(token: string, id: number): Promise<void> {
  await request(token, `/rules/${id}`, "DELETE");
}

export async function getStatus(token: string): Promise<IngestorStatus> {
  const res = await request(token, "/status", "GET");
  return (await res.json()) as IngestorStatus;
}

// ── react-query hooks ───────────────────────────────────────────────────
// `getToken` is `useAdminAuth().getValidToken` when signed in, `null`
// before — the queries stay disabled (and render empty) until sign-in.
// retry: false so a dead JWT doesn't hammer the service.

export function useIngestorPackages(getToken: (() => Promise<string>) | null) {
  return useQuery<TrackedPackageDto[], Error>({
    queryKey: ["ingestor-packages"],
    queryFn: async () => listPackages(await getToken!()),
    enabled: getToken !== null,
    retry: false,
  });
}

export function useIngestorRules(getToken: (() => Promise<string>) | null) {
  return useQuery<RuleDto[], Error>({
    queryKey: ["ingestor-rules"],
    queryFn: async () => listRules(await getToken!()),
    enabled: getToken !== null,
    retry: false,
  });
}

export function useIngestorStatus(getToken: (() => Promise<string>) | null) {
  return useQuery<IngestorStatus, Error>({
    queryKey: ["ingestor-status"],
    queryFn: async () => getStatus(await getToken!()),
    enabled: getToken !== null,
    refetchInterval: 15_000,
    retry: false,
  });
}
