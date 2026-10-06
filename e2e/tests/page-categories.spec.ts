import { test, expect, Page } from "@playwright/test";

// #842 — pages can be filed under categories from the page editor, on
// create and on edit. #848 — such a page (with a revision, a tag and a
// category) can then be deleted; the delete used to fail on its FKs.
test.describe.configure({ mode: "serial" });

async function createCategory(page: Page, name: string, parent?: string) {
  await page.goto("/cms-admin/taxonomies/category/categories/new");
  await page.locator('main input[name="name"]').fill(name);
  if (parent) await page.locator('main select[name="parent_id"]').selectOption({ label: parent });
  await page.locator("main form button", { hasText: "Create category" }).click();
  await page.waitForURL(/\/categories\/\d+\/edit/);
}

async function promoteTab(page: Page) {
  await page.locator('[role="tab"][aria-controls="tab-promote"]').click();
}

test("a page is filed under categories on create and edit, then deleted", async ({ page }) => {
  const stamp = Date.now();
  const mugs = `Mugs ${stamp}`;
  const cups = `Espresso cups ${stamp}`;
  const plates = `Plates ${stamp}`;
  await createCategory(page, mugs);
  await createCategory(page, cups, mugs);
  await createCategory(page, plates);

  // Create: title on the Content tab, a tag and a category on Promote.
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('[role="tab"][aria-controls="tab-content"]').click();
  await page.locator('input[name="title"]').fill(`Filed ${stamp}`);
  await page.locator('input[name="slug"]').fill(`filed-${stamp}`);
  await promoteTab(page);
  await page.locator('input[name="tags"]').fill(`t${stamp}`);
  const group = page.locator('[data-name="categories"]');
  await group.locator("label", { hasText: cups }).locator("input").check();
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);

  // Edit: the create saved it; add a second one.
  await page.getByRole("link", { name: `Filed ${stamp}` }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  const id = page.url().match(/\/pages\/(\d+)\/edit/)![1];
  await promoteTab(page);
  await expect(group.locator("label", { hasText: cups }).locator("input")).toBeChecked();
  await expect(page.locator('input[name="tags"]'), "tags saved on create").toHaveValue(`t${stamp}`);
  await group.locator("label", { hasText: plates }).locator("input").check();
  await page.locator('button[form="page-edit-form"][value="continue"]').click();
  await page.waitForURL(new RegExp(`/cms-admin/pages/${id}/edit`));
  await promoteTab(page);
  await expect(group.locator("label", { hasText: plates }).locator("input")).toBeChecked();
  await expect(group.locator("label", { hasText: mugs }).locator("input")).not.toBeChecked();

  // Delete: the page has a revision, a tag and two categories.
  const csrf = (await page.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
  const res = await page.request.post(`/cms-admin/pages/${id}/delete`, { form: { _csrf: csrf }, maxRedirects: 0 });
  expect([302, 303], "delete redirects, not 500").toContain(res.status());
  expect((await page.request.get(`/filed-${stamp}`)).status(), "the page is gone").toBe(404);
});
