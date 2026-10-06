import { test, expect } from "@playwright/test";
import { DatabaseSync } from "node:sqlite";

// The schedule inputs take the editor's wall time in their admin timezone
// and store UTC. They used to store the typed time as if it were UTC, so
// "publish at 09:00" in Halifax fired at 06:00 local time.
const DB = process.env.E2E_DB ?? ".state/demo.db";
test.use({ timezoneId: "America/Halifax" });

test("Go live / Expire at are typed in the editor's timezone and stored as UTC", async ({ page }) => {
  const stamp = Date.now();
  const slug = `sale-${stamp}`;
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(`Sale ${stamp}`);
  await page.locator('input[name="slug"]').fill(slug);
  await page.locator('button[form="page-edit-form"][value="continue"]').click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  const id = Number(page.url().match(/pages\/(\d+)/)![1]);

  // December: Halifax is UTC−4 (AST).
  await page.locator('[data-tab-trigger="promote"]').click();
  await page.locator('input[name="go_live_at"]').fill("2030-12-01T09:00");
  await page.locator('input[name="expire_at"]').fill("2030-12-08T18:30");
  await page.locator('button[form="page-edit-form"][value="continue"]').click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);

  const db = new DatabaseSync(DB);
  const row = db.prepare("SELECT status, go_live_at, expire_at FROM cms_page WHERE id = ?").get(id) as Record<string, string>;
  db.close();
  expect(new Date(row.go_live_at).toISOString()).toBe("2030-12-01T13:00:00.000Z");
  expect(new Date(row.expire_at).toISOString()).toBe("2030-12-08T22:30:00.000Z");
  expect(row.status).toBe("scheduled");

  // Shown back in the editor's time, with the zone named.
  await page.locator('[data-tab-trigger="promote"]').click();
  await expect(page.locator('input[name="go_live_at"]')).toHaveValue("2030-12-01T09:00");
  await expect(page.locator('input[name="expire_at"]')).toHaveValue("2030-12-08T18:30");
  await expect(page.locator("#tab-promote [data-user-tz]").first()).toContainText("America/Halifax");

  // Not live before its time.
  expect((await page.request.get(`/${slug}`)).status()).toBe(404);
});
