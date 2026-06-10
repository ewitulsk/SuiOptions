// Verified-orgs allowlist admin client — the authenticated mutate side of
// token-info's `/orgs` routes.
//
// Orgs are created permissionlessly on-chain; this allowlist (platform-
// controlled) decides which orgs' buckets api-service /buckets, the quoting
// service, and this frontend serve. Reads are open; writes carry the admin
// JWT (same auth-service flow as the token catalog — see useAdminAuth).

import { TOKEN_INFO_URL } from "../config";
import { AuthExpiredError } from "./tokenAdmin";

const base = TOKEN_INFO_URL.replace(/\/$/, "");

/** Wire shape of an allowlist row as token-info serializes it. */
export type VerifiedOrgDto = {
  org_id: string;
  name: string;
  enabled: boolean;
};

/** Full allowlist (enabled and disabled), as served by `GET /orgs`. */
export async function listVerifiedOrgs(): Promise<VerifiedOrgDto[]> {
  const res = await fetch(`${base}/orgs`);
  if (!res.ok) throw new Error(`token-info /orgs → ${res.status}`);
  return (await res.json()) as VerifiedOrgDto[];
}

async function mutate(
  token: string,
  path: string,
  method: "POST" | "PUT" | "DELETE",
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
    throw new Error(`${method} ${path} → ${res.status}: ${await res.text()}`);
  }
  return res;
}

/** Verify (add or replace) an org on the allowlist. */
export async function verifyOrg(
  token: string,
  input: VerifiedOrgDto,
): Promise<VerifiedOrgDto> {
  const res = await mutate(token, "/orgs", "POST", input);
  return (await res.json()) as VerifiedOrgDto;
}

/** Flip an org's enabled flag (de-verify keeps history; prefer over DELETE). */
export async function setOrgEnabled(
  token: string,
  org: VerifiedOrgDto,
  enabled: boolean,
): Promise<VerifiedOrgDto> {
  const res = await mutate(
    token,
    `/orgs/${encodeURIComponent(org.org_id)}`,
    "PUT",
    { ...org, enabled },
  );
  return (await res.json()) as VerifiedOrgDto;
}

/** Remove an org from the allowlist entirely. */
export async function deleteVerifiedOrg(token: string, orgId: string): Promise<void> {
  await mutate(token, `/orgs/${encodeURIComponent(orgId)}`, "DELETE");
}
