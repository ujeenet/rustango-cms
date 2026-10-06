import { test, expect } from "@playwright/test";

// #679 — an autosave must not half-apply a slug change. It used to write
// the new slug to the live row without url_path, so the next real save
// saw "no slug change" and never moved the URL or left a 301 behind.
test.describe.configure({ mode: "serial" });

test("a slug edited before an autosave still renames the page on save", async ({ page }) => {
  const stamp = Date.now();
  const before = `autosave-before-${stamp}`;
  const after = `autosave-after-${stamp}`;

  // A published root page at /<before>.
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  // Not the Home type: the API specs assert the exact set of Home pages in
  // this shared tenant.
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(`Autosave ${stamp}`);
  await page.locator('input[name="slug"]').fill(before);
  await page.locator('select[name="status"]').selectOption("published");
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);
  expect((await page.request.get(`/${before}`)).status()).toBe(200);

  // Edit the slug, then let the autosave fire before saving for real.
  await page.getByRole("link", { name: `Autosave ${stamp}` }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  await page.locator('input[name="slug"]').fill(after);
  const autosave = await page.evaluate(async () => {
    const form = document.getElementById("page-edit-form") as HTMLFormElement;
    const csrf = (document.cookie.split(";").find((c) => c.trim().startsWith("rustango_csrf=")) || "").split("=")[1] || "";
    const res = await fetch(form.dataset.autosaveUrl as string, {
      method: "POST",
      body: new URLSearchParams(new FormData(form) as unknown as Record<string, string>),
      headers: { "X-CSRF-Token": csrf },
      credentials: "same-origin",
    });
    return res.status;
  });
  expect(autosave, "autosave accepted").toBeLessThan(300);

  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForLoadState("networkidle");

  // The URL followed the slug, and the old one redirects to it.
  expect((await page.request.get(`/${after}`)).status()).toBe(200);
  const old = await page.request.get(`/${before}`, { maxRedirects: 0 });
  expect(old.status()).toBe(301);
  expect(old.headers()["location"] ?? "").toContain(`/${after}`);
});
