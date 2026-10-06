import { test, expect, Page } from "@playwright/test";

// #843 — a page type made in the admin keeps the parent rules entered on
// its form, and can be bound to a workflow; its pages can then be
// submitted for review (they used to fail with "no registered handler").
test.describe.configure({ mode: "serial" });

async function csrf(page: Page) {
  return (await page.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
}

test("an admin-made type honours its parent rule and its workflow", async ({ page }) => {
  const stamp = Date.now();
  const workflow = `Shop review ${stamp}`;

  // A workflow with one step.
  await page.goto("/cms-admin/workflows/new");
  await page.locator('input[name="name"]').first().fill(workflow);
  await page.locator('input[name="active"]').check();
  await page.locator('form[action*="/workflows"] button[type="submit"]').first().click();
  await page.waitForURL(/\/cms-admin\/workflows\/\d+\/edit/);
  const taskForm = page.locator('form[action*="/tasks/add"]');
  await taskForm.locator('input[name="name"]').fill("Owner approves");
  await taskForm.locator('select[name="role_id"]').selectOption({ index: 1 });
  await taskForm.locator('button[type="submit"]').click();
  await page.waitForLoadState("networkidle");

  // A type that may only live under a Sectioned page, bound to it.
  await page.goto("/cms-admin/page-types/new");
  await page.locator('input[name="verbose_name"]').fill(`Product ${stamp}`);
  await page.locator('input[name="type_name"]').fill(`product_${stamp}`);
  await page.locator('input[name="allowed_parents"]').fill("SectionedPage");
  await page.locator('select[name="workflow"]').selectOption(workflow);
  await page.locator('form[action$="/page-types/new"] button[type="submit"]').click();
  await page.waitForURL(/\/page-types\/\d+\/build/);
  const typeId = page.url().match(/\/page-types\/(\d+)\/build/)![1];

  // Not offered at the root…
  await page.goto("/cms-admin/pages/new");
  await expect(page.getByText(`Product ${stamp}`)).toHaveCount(0);
  // …and refused there even when asked for directly.
  const root = await page.request.post("/cms-admin/pages/new", {
    form: { _csrf: await csrf(page), page_type_id: typeId, title: `Root product ${stamp}`, slug: `root-product-${stamp}`, status: "draft" },
    maxRedirects: 0,
  });
  expect(root.status(), "a root page of this type is refused").toBe(400);

  // Offered under a Sectioned page.
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('[role="tab"][aria-controls="tab-content"]').click();
  await page.locator('input[name="title"]').fill(`Shop ${stamp}`);
  await page.locator('input[name="slug"]').fill(`shop-${stamp}`);
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);
  const shopHref = await page.getByRole("link", { name: `Shop ${stamp}` }).first().getAttribute("href");
  const shopId = shopHref!.match(/\/pages\/(\d+)\/edit/)![1];
  await page.goto(`/cms-admin/pages/new?parent=${shopId}`);
  await expect(page.getByText(`Product ${stamp}`).first()).toBeVisible();

  // A page of the type, then a submit for review.
  await page.goto(`/cms-admin/pages/new?type=${typeId}&parent=${shopId}`);
  await page.locator('[role="tab"][aria-controls="tab-content"]').click();
  await page.locator('input[name="title"]').fill(`Blue mug ${stamp}`);
  await page.locator('button[form="page-edit-form"][value="continue"]').click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  const productId = page.url().match(/\/pages\/(\d+)\/edit/)![1];
  const submit = await page.request.post(`/cms-admin/pages/${productId}/workflow/submit`, {
    form: { _csrf: await csrf(page) },
  });
  const body = await submit.text();
  expect(body, "submitted into the type's workflow").toContain("Submitted for review");
  expect(body).toContain("Owner approves");

  // The settings screen shows and saves the choice.
  await page.goto(`/cms-admin/page-types/${typeId}/edit`);
  await expect(page.locator('select[name="workflow"]')).toHaveValue(workflow);
  await expect(page.locator('input[name="type_name"]')).toHaveAttribute("readonly", "");
  await page.locator('select[name="workflow"]').selectOption("");
  await page.locator('form[action*="/edit"] button[type="submit"]').click();
  await page.waitForURL(/\/cms-admin\/page-types$/);
  await page.goto(`/cms-admin/page-types/${typeId}/edit`);
  await expect(page.locator('select[name="workflow"]')).toHaveValue("");
});
