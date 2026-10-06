import { defineConfig, devices } from "@playwright/test";

// The suite drives the cms_demo example on a fresh sqlite state dir —
// see serve.sh. Chromium resolves *.localhost to loopback, so
// demo.localhost:<port> reaches the tenant without /etc/hosts edits.
const PORT = Number(process.env.E2E_PORT || 8210);
const BASE = `http://demo.localhost:${PORT}`;

export default defineConfig({
  testDir: "./tests",
  // Specs share one server + one tenant DB; files run in parallel workers
  // but each spec file owns its data (pages/media it creates), and the
  // media-picker spec is serial internally.
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 60_000,
  reporter: [["list"], ["html", { open: "never" }]],
  use: {
    baseURL: BASE,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: {
    command: "bash serve.sh",
    url: `${BASE}/login`,
    // First run compiles the example — allow a long warm-up.
    timeout: 600_000,
    reuseExistingServer: !process.env.CI,
    stdout: "ignore",
    stderr: "pipe",
  },
  projects: [
    // Logs in once and saves the session for every other project.
    { name: "setup", testMatch: /auth\.setup\.ts/ },
    {
      name: "chromium",
      testIgnore: /api\//,
      use: {
        ...devices["Desktop Chrome"],
        storageState: ".state/admin.json",
      },
      dependencies: ["setup"],
    },

    // ---- API suite -------------------------------------------------
    // Seeds the fixture and registers a member. Depends on `setup` only
    // for the admin session the member registration may need to fall
    // back on.
    {
      name: "api-setup",
      testMatch: /api\/api\.setup\.ts/,
      use: { ...devices["Desktop Chrome"] },
      dependencies: ["setup"],
    },
    // The important one: no cookies at all. Every "does this leak?"
    // assertion runs here, and a stray storageState would silently make
    // the whole leak matrix vacuous.
    // Untagged specs run here and only here, so "no cookies" is the
    // default posture rather than something each spec has to remember.
    {
      name: "api-anon",
      testMatch: /api\/.*\.spec\.ts/,
      grepInvert: /@member|@admin/,
      use: { storageState: { cookies: [], origins: [] } },
      dependencies: ["api-setup"],
    },
    {
      name: "api-member",
      testMatch: /api\/.*\.spec\.ts/,
      grep: /@member/,
      use: { storageState: ".state/member.json" },
      dependencies: ["api-setup"],
    },
    {
      name: "api-admin",
      testMatch: /api\/.*\.spec\.ts/,
      grep: /@admin/,
      use: { ...devices["Desktop Chrome"], storageState: ".state/admin.json" },
      dependencies: ["api-setup"],
    },
  ],
});
