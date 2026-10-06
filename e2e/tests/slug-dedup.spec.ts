import { test, expect, Page } from "@playwright/test";

// #703 — editing a page's slug onto a sibling's used to give both rows the
// same URL; the edit is now refused and the page keeps its old URL.
test.describe.configure({ mode: "serial" });

async function createRoot(page: Page, title: string, slug: string) {
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(title);
  await page.locator('input[name="slug"]').fill(slug);
  await page.locator('select[name="status"]').selectOption("published");
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);
}

test("a slug edit onto a sibling's slug is refused", async ({ page }) => {
  const stamp = Date.now();
  const taken = `dedup-a-${stamp}`;
  const mine = `dedup-b-${stamp}`;
  await createRoot(page, `Dedup A ${stamp}`, taken);
  await createRoot(page, `Dedup B ${stamp}`, mine);

  await page.getByRole("link", { name: `Dedup B ${stamp}` }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  await page.locator('input[name="slug"]').fill(taken);
  const [res] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === "POST" && /\/cms-admin\/pages\/\d+\/edit/.test(r.url())),
    page.locator('button[form="page-edit-form"][value="save"]').click(),
  ]);
  expect(res.status()).toBe(400);
  expect(await res.text()).toContain(`already uses the slug “${taken}”`);

  expect((await page.request.get(`/${mine}`)).status(), "the page kept its URL").toBe(200);
  expect(await (await page.request.get(`/${taken}`)).text()).toContain(`Dedup A ${stamp}`);
});

// A new root page with an empty slug becomes the front page, as the form's
// hint says; a second one takes its slug from the title instead.
test("an empty slug on a new root page makes it the front page, once", async ({ page }) => {
  const stamp = Date.now();
  await createRoot(page, `Front ${stamp}`, "");
  expect(await (await page.request.get("/")).text()).toContain(`Front ${stamp}`);

  await createRoot(page, `Second front ${stamp}`, "");
  expect((await page.request.get(`/second-front-${stamp}`)).status(), "slug from the title").toBe(200);
  expect(await (await page.request.get("/")).text(), "the first keeps /").toContain(`Front ${stamp}`);
});
