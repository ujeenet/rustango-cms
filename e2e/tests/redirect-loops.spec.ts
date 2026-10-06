import { test, expect, Page } from "@playwright/test";

// #708 — nothing saves a redirect that sends a visitor back into itself.
test.describe.configure({ mode: "serial" });

async function exportedRules(page: Page): Promise<string[][]> {
  const csv = await (await page.request.get("/cms-admin/redirects/export.csv")).text();
  return csv.split("\n").map((l) => l.split(",").map((c) => c.trim()));
}

async function addRule(page: Page, from: string, to: string) {
  await page.goto("/cms-admin/redirects/new");
  await page.locator('input[name="from_path"]').fill(from);
  await page.locator('input[name="to_path"]').fill(to);
  await page.locator('form[action*="/cms-admin/redirects/new"] button[type="submit"]').click();
  await page.waitForLoadState("networkidle");
}

test("a self-targeting or self-matching rule is refused", async ({ page }) => {
  const stamp = Date.now();
  const exact = `/loop-${stamp}`;
  const wild = `/loopdir-${stamp}/*`;
  await addRule(page, exact, exact);
  await expect(page.getByText(/points back at its own from-path/i)).toBeVisible();
  await addRule(page, wild, `/loopdir-${stamp}/archive/*`);
  await expect(page.getByText(/points back at its own from-path/i)).toBeVisible();

  const rules = await exportedRules(page);
  expect(rules.some((r) => r[0] === exact)).toBe(false);
  expect(rules.some((r) => r[0] === wild)).toBe(false);
});

test("renaming a page back does not chain a self-redirect", async ({ page }) => {
  const stamp = Date.now();
  const a = `rename-a-${stamp}`;
  const b = `rename-b-${stamp}`;
  const title = `Rename back ${stamp}`;

  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(title);
  await page.locator('input[name="slug"]').fill(a);
  await page.locator('select[name="status"]').selectOption("published");
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);

  for (const slug of [b, a]) {
    await page.getByRole("link", { name: title }).first().click();
    await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
    await page.locator('input[name="slug"]').fill(slug);
    await page.locator('button[form="page-edit-form"][value="save"]').click();
    await page.waitForURL(/\/cms-admin\/pages(?!\/\d+\/edit)/);
  }

  const rules = await exportedRules(page);
  expect(rules.filter((r) => r[0] === `/${a}` && r[1] === `/${a}`), "no /a → /a rule").toHaveLength(0);
  expect(rules.some((r) => r[0] === `/${b}` && r[1] === `/${a}`), "the /b → /a rule stays").toBe(true);
  expect((await page.request.get(`/${a}`)).status()).toBe(200);
});
