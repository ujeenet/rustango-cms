/**
 * The member the suite registers and signs in as.
 *
 * Lives here rather than in `api.setup.ts` because Playwright refuses to
 * let a spec import a setup file, and both need these values.
 */
export const MEMBER_EMAIL = "api-tester@example.com";
export const MEMBER_PASSWORD = "MemberPw123!";
