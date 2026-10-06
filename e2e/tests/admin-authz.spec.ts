import { test, expect, type Page } from "@playwright/test";

// Admin authorization: who may use /cms-admin at all (#671), and who may
// administer identity — roles and SSO providers (#672). Each case signs in
// fresh, so it runs without the shared superuser session.
test.use({ storageState: { cookies: [], origins: [] } });

async function signIn(page: Page, username: string, password: string) {
  await page.goto("/login");
  await page.locator('input[name="username"]').fill(username);
  await page.locator('input[type="password"]').fill(password);
  await page.locator('button[type="submit"]').click();
  // Wait for the login POST's redirect to land; navigating earlier aborts it.
  await page.waitForURL((url) => !url.pathname.startsWith("/login"));
}

test.describe("admin authorization", () => {
  test("a self-registered member cannot use the admin", async ({ page }) => {
    const email = `member-${Date.now()}@example.test`;
    await page.goto("/members/signup");
    await page.locator('input[name="email"]').fill(email);
    await page.locator('input[name="password"]').fill("Member-pass-12345");
    await page.locator('input[name="password_confirm"]').fill("Member-pass-12345");
    const name = page.locator('input[name="display_name"]');
    if (await name.count()) await name.fill("A Member");
    await page.locator('form[action*="signup"] button[type="submit"]').click();
    await page.waitForLoadState("networkidle");

    await signIn(page, email, "Member-pass-12345");

    // Every admin surface — reading and writing — sends a member away.
    for (const path of ["/cms-admin/pages", "/cms-admin/pages/new", "/cms-admin/redirects/new", "/cms-admin/roles"]) {
      await page.goto(path);
      await expect(page, `${path} must not be reachable by a member`).toHaveURL(/\/cms-admin\/no-access/);
    }
  });

  test("an Editor reaches the admin but not roles or SSO providers", async ({ page: admin, browser, baseURL }) => {
    await signIn(admin, "admin", "TestPw123!");
    const page = await userWithRole(admin, browser, baseURL, "Editor");

    // Staff keep the admin: the #671 gate must not lock out seeded roles.
    await page.goto("/cms-admin/pages");
    await expect(page).toHaveURL(/\/cms-admin\/pages/);

    // Identity administration is superuser-only (#672).
    for (const path of ["/cms-admin/roles", "/cms-admin/roles/new", "/cms-admin/sso-providers", "/cms-admin/sso-providers/new"]) {
      await page.goto(path);
      await expect(page, `${path} must be superuser-only`).toHaveURL(/\/cms-admin\/no-access/);
    }

    // The write handlers are gated too, not just the pages that link to them.
    const csrf = (await page.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
    const res = await page.request.post("/cms-admin/roles/new", {
      form: { _csrf: csrf, name: "Escalated" },
      maxRedirects: 0,
    });
    expect([302, 303]).toContain(res.status());
    expect(res.headers()["location"] ?? "").toContain("/cms-admin/no-access");
    await page.context().close();
  });

  test("page rights follow the role matrix, not just admin access", async ({ page: admin, browser, baseURL }) => {
    await signIn(admin, "admin", "TestPw123!");
    const stamp = Date.now();
    await admin.goto("/cms-admin/pages");
    await admin.getByRole("link", { name: /new root page/i }).click();
    await admin.getByRole("link", { name: /sectioned page/i }).first().click();
    await admin.locator('input[name="title"]').fill(`Authz ${stamp}`);
    await admin.locator('input[name="slug"]').fill(`authz-${stamp}`);
    await admin.locator('button[form="page-edit-form"][value="save"]').click();
    await admin.waitForURL(/\/cms-admin\/pages(?!\/new)/);
    // Read the id off the list link: opening the editor would take the
    // editor lock and make the other users' saves fail for that reason.
    const href = await admin.getByRole("link", { name: `Authz ${stamp}` }).first().getAttribute("href");
    const id = href!.match(/\/pages\/(\d+)\/edit/)![1];

    const post = async (who: Page, path: string, form: Record<string, string> = {}) => {
      const csrf = (await who.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
      return (await who.request.post(path, { form: { _csrf: csrf, ...form }, maxRedirects: 0 })).status();
    };

    // Viewer: reads, but every write is refused.
    const viewer = await userWithRole(admin, browser, baseURL, "Viewer");
    await viewer.goto("/cms-admin/pages");
    await expect(viewer).toHaveURL(/\/cms-admin\/pages/);
    expect(await post(viewer, `/cms-admin/pages/${id}/edit`, { title: "Viewer edit", slug: `authz-${stamp}`, status: "draft" })).toBe(403);
    expect(await post(viewer, `/cms-admin/pages/${id}/delete`)).toBe(403);
    expect(await post(viewer, "/cms-admin/redirects/new", { from_path: `/v-${stamp}`, to_path: "/" })).toBe(403);
    await viewer.context().close();

    // Editor: edits, but may neither delete nor publish.
    const editor = await userWithRole(admin, browser, baseURL, "Editor");
    await editor.goto(`/cms-admin/pages/${id}/edit`);
    expect(await post(editor, `/cms-admin/pages/${id}/delete`)).toBe(403);
    expect([302, 303]).toContain(await post(editor, "/cms-admin/pages/bulk", { action: "delete", ids: id }));
    const publish = await editor.evaluate(async () => {
      const form = document.getElementById("page-edit-form") as HTMLFormElement;
      const data = new FormData(form);
      data.set("status", "published");
      data.set("_action", "save");
      const res = await fetch(form.action, {
        method: "POST",
        body: new URLSearchParams(data as unknown as Record<string, string>),
        credentials: "same-origin",
      });
      return res.url;
    });
    expect(publish, "publish refused back onto the editor").toMatch(/\/cms-admin\/pages\/\d+\/edit/);
    await editor.context().close();

    // None of those went through: still a draft, still titled as created.
    const live = await admin.request.get(`/authz-${stamp}`);
    expect(live.status(), "never published").toBe(404);
    await admin.goto("/cms-admin/pages");
    await expect(admin.getByRole("link", { name: `Authz ${stamp}` }).first()).toBeVisible();
  });

  // #709 — the codename gates in front of template authoring, sites,
  // notifications and the page-type builder, and (#646) the superuser gate
  // on another user's MCP keys: denied without the codename, open with it.
  test("codename gates send a user without the codename to no-access", async ({ page: admin, browser, baseURL }) => {
    await signIn(admin, "admin", "TestPw123!");
    const gated = ["/cms-admin/templates", "/cms-admin/sites", "/cms-admin/notifications", "/cms-admin/page-types/1/build"];

    const editor = await userWithRole(admin, browser, baseURL, "Editor");
    for (const path of gated) {
      await editor.goto(path);
      await expect(editor, `${path} needs its codename`).toHaveURL(/\/cms-admin\/no-access/);
    }
    const csrf = (await editor.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
    const writes: [string, Record<string, string>][] = [
      ["/cms-admin/templates/new", { name: "blocks/x.html" }],
      ["/cms-admin/sites/new", { hostname: "evil.example" }],
      ["/cms-admin/notifications/new", { label: "x", kind: "webhook" }],
      ["/cms-admin/users/1/mcp-keys", { name: "escalate" }],
      ["/cms-admin/users/1/mcp-keys/1/revoke", {}],
    ];
    for (const [path, form] of writes) {
      const res = await editor.request.post(path, { form: { _csrf: csrf, ...form }, maxRedirects: 0 });
      expect([302, 303, 403], `${path} status`).toContain(res.status());
      if (res.status() !== 403) {
        expect(res.headers()["location"] ?? "", `${path} redirect`).toContain("/cms-admin/no-access");
      }
    }
    await editor.context().close();

    // The seeded Developer role holds all four codenames.
    const developer = await userWithRole(admin, browser, baseURL, "Developer");
    for (const path of gated) {
      await developer.goto(path);
      await expect(developer, `${path} opens with the codename`).not.toHaveURL(/\/cms-admin\/no-access/);
    }
    await developer.context().close();
  });

  // #651 — the superuser gate (#642) is wired on every user-management
  // handler, not just decided correctly: the strongest seeded non-superuser
  // role is refused on each route, and its attempt to mint a superuser
  // leaves no account behind.
  test("user management refuses a signed-in non-superuser on every route", async ({ page: admin, browser, baseURL }) => {
    await signIn(admin, "admin", "TestPw123!");
    const developer = await userWithRole(admin, browser, baseURL, "Developer");

    for (const path of ["/cms-admin/users", "/cms-admin/users/new", "/cms-admin/users/1/edit"]) {
      await developer.goto(path);
      await expect(developer, `GET ${path} must be superuser-only`).toHaveURL(/\/cms-admin\/no-access/);
    }

    const escalated = `escalated${Date.now()}`;
    const csrf = (await developer.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
    const writes: [string, Record<string, string>][] = [
      ["/cms-admin/users/new", { username: escalated, email: `${escalated}@example.test`, password: "Escalate-pass-12345", is_superuser: "on", active: "on" }],
      ["/cms-admin/users/1/edit", { username: "admin", email: "admin@example.test", is_superuser: "on", active: "on" }],
      ["/cms-admin/users/1/deactivate", {}],
      ["/cms-admin/users/bulk", { action: "disable", user_id: "1" }],
    ];
    for (const [path, form] of writes) {
      const res = await developer.request.post(path, { form: { _csrf: csrf, ...form }, maxRedirects: 0 });
      expect([302, 303], `POST ${path} status`).toContain(res.status());
      expect(res.headers()["location"] ?? "", `POST ${path} redirect`).toContain("/cms-admin/no-access");
    }
    await developer.context().close();

    // Nothing went through: no new account, and the admin still signs in.
    await admin.goto("/cms-admin/users");
    await expect(admin.locator(`input[name="user_id"][aria-label="Select ${escalated}"]`)).toHaveCount(0);
    await expect(admin.locator('input[name="user_id"][aria-label="Select admin"]')).toHaveCount(1);
  });
});

// A fresh user holding the seeded role `role`, signed in in its own
// browser context. `admin` is a signed-in superuser page.
async function userWithRole(admin: Page, browser: import("@playwright/test").Browser, baseURL: string | undefined, role: string) {
  const username = `${role.toLowerCase()}${Date.now()}`;
  await admin.goto("/cms-admin/users/new");
  await admin.locator('input[name="username"]').fill(username);
  await admin.locator('input[name="email"]').fill(`${username}@example.test`);
  await admin.locator('input[name="password"]').fill("Role-pass-12345");
  await admin.locator('form[action*="/cms-admin/users"] button[type="submit"]').first().click();
  await admin.waitForLoadState("networkidle");
  // Roles are granted on the edit form, not the create form.
  await admin.goto("/cms-admin/users");
  const userId = await admin.locator(`input[name="user_id"][aria-label="Select ${username}"]`).getAttribute("value");
  await admin.goto(`/cms-admin/users/${userId}/edit`);
  await admin.locator(`label:has(strong:text-is("${role}")) input[name="role_id"]`).check();
  // Save, not Deactivate: that button submits its own form (form="…").
  await admin.locator('form[data-primary-save] button[type="submit"]:not([form])').click();
  await admin.waitForLoadState("networkidle");

  // A separate browser context, so the user's session is its own.
  const context = await browser.newContext({ baseURL });
  const page = await context.newPage();
  await signIn(page, username, "Role-pass-12345");
  return page;
}
