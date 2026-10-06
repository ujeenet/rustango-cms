import { test, expect } from "@playwright/test";

// #712 — a hostname outside the operator's own domains is added disabled
// and stays that way until an operator approves it: a tenant must not be
// able to route someone else's domain to itself by claiming it first.
test("a claimed hostname waits for operator approval", async ({ page }) => {
  const host = `shop-${Date.now()}.example-customer.test`;
  await page.goto("/cms-admin/sites");
  const csrf = (await page.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";

  const add = await page.request.post("/cms-admin/sites/new", {
    form: { _csrf: csrf, hostname: host, root_page_id: "" },
    maxRedirects: 0,
  });
  expect([302, 303], await add.text()).toContain(add.status());

  await page.goto("/cms-admin/sites");
  const row = page.locator("tr", { hasText: host });
  await expect(row).toContainText("awaiting operator approval");
  await expect(row.locator('form[action="/cms-admin/sites/toggle"]')).toHaveCount(0);

  // Posting the toggle by hand is refused too, even for a superuser.
  const enable = await page.request.post("/cms-admin/sites/toggle", {
    form: { _csrf: csrf, hostname: host },
    maxRedirects: 0,
  });
  expect(enable.status()).toBeGreaterThanOrEqual(400);
  await page.goto("/cms-admin/sites");
  await expect(page.locator("tr", { hasText: host })).toContainText("awaiting operator approval");
});
