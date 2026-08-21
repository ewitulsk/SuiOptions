// Service endpoints. Defaults point at staging, which is the only environment
// this dashboard is ever deployed against — dakota-service talks to Dakota's
// SANDBOX and is deliberately absent from the prod compose file.

export const DAKOTA_API = (
  import.meta.env.VITE_DAKOTA_API ?? "https://sui-options.com/staging/dakota"
).replace(/\/$/, "");

export const AUTH_API = (
  import.meta.env.VITE_AUTH_API ?? "https://sui-options.com/staging/auth"
).replace(/\/$/, "");

/// Dakota's sandbox refuses anything above $2.00 per transaction. Surfaced in
/// the UI so the limit is visible before a form is submitted rather than
/// arriving as a rejection afterwards.
export const SANDBOX_MAX_AMOUNT = 2.0;
