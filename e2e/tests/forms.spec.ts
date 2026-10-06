import { test, expect } from "@playwright/test";

// The dedicated Forms area: its own list (not the Library), correct nav
// highlight, per-form submission column, and the builder's Settings /
// Preview panels replacing the canvas (the [hidden]-vs-display fix).
test.describe.configure({ mode: "serial" });

test.describe("forms area", () => {
  test("rcms-sidebar Forms opens the dedicated list with its rcms-empty state", async ({ page }) => {
    await page.goto("/cms-admin/dashboard");
    await page.locator('a.rcms-sidebar-link[href="/cms-admin/forms"]').click();
    await page.waitForURL(/\/cms-admin\/forms$/);
    await expect(page.getByRole("heading", { name: /forms/i }).first()).toBeVisible();
    // Forms nav item highlighted, not Library.
    await expect(page.locator('a.rcms-sidebar-link[href="/cms-admin/forms"]')).toHaveClass(/active/);
    await expect(page.getByText(/no forms yet/i)).toBeVisible();
  });

  test("New form creates a snippet and lands in the visual builder", async ({ page }) => {
    await page.goto("/cms-admin/forms");
    await page.getByRole("link", { name: /new form/i }).first().click();
    // Generic new-snippet form (type preset to form) → create.
    await page.locator('input[name="title"]').fill("E2E Contact");
    await expect(page.locator('textarea[name="body_markdown"]')).toHaveCount(0); // no body for forms
    await page.locator('.rcms-app-content button[type="submit"]').first().click();
    // Create opens the new form's builder straight away…
    await page.waitForURL(/\/cms-admin\/forms\/\d+\/build/);
    await expect(page.locator("#form-builder-root")).toBeVisible();
    // …and the Forms list has the row, whose Build link opens it again.
    await page.goto("/cms-admin/forms");
    const row = page.locator("table tbody tr", { hasText: "E2E Contact" });
    await expect(row).toBeVisible();
    await row.getByRole("link", { name: /build/i }).click();
    await page.waitForURL(/\/cms-admin\/forms\/\d+\/build/);
    await expect(page.locator("#form-builder-root")).toBeVisible();
  });

  test("builder Settings replaces the canvas; Preview is a sidebar pane", async ({ page }) => {
    await page.goto("/cms-admin/forms");
    await page.locator('a[href*="/build"]').first().click();
    await page.waitForURL(/\/build/);

    const workspace = page.locator(".rcms-fb-workspace");
    const settings = page.locator("#fb-settings");
    await expect(workspace).toBeVisible();

    // Settings still takes over the canvas slot.
    await page.locator("#fb-settings-toggle").click();
    await expect(settings).toBeVisible();
    await expect(workspace).toBeHidden(); // the [hidden]-vs-display:grid fix
    await expect(settings).toContainText(/form settings/i);
    await page.locator("#fb-settings-toggle").click();
    await expect(workspace).toBeVisible();

    // Preview does NOT: it is the shared sidebar pane (the same one the
    // page editor uses), so the builder stays on screen beside it. This
    // replaced a preview that re-rendered the form in JavaScript.
    const pane = page.locator(".rcms-preview-pane");
    await expect(pane).toHaveCount(1);
    await expect(page.locator("#fb-preview")).toHaveCount(0);

    const toggle = page.locator("[data-preview-toggle]").first();
    for (let i = 0; i < 3; i++) {
      if (!(await page.evaluate(() => document.documentElement.hasAttribute("data-preview-hidden")))) break;
      await toggle.click();
      await page.waitForTimeout(300);
    }
    await expect(workspace).toBeVisible(); // pane and builder coexist

    // And it renders through the real server-side renderer, not a
    // client-side approximation: the frame carries the actual submit
    // target and the honeypot the public form has.
    const frame = page.frameLocator("iframe.rcms-preview-frame");
    await expect(frame.locator("form")).toHaveAttribute("action", /\/forms\/submit\/\d+/);
    await expect(frame.locator('input[name="_hp"]')).toHaveCount(1);
  });

  test("forms list shows the form with a zero-submission dash", async ({ page }) => {
    await page.goto("/cms-admin/forms");
    const row = page.locator("table tbody tr", { hasText: "E2E Contact" });
    await expect(row).toBeVisible();
    await expect(row).toContainText("—"); // no submissions yet
    await expect(row.getByRole("link", { name: /submissions/i })).toBeVisible();
  });

  test("submissions view opens with its rcms-empty state", async ({ page }) => {
    await page.goto("/cms-admin/forms");
    await page
      .locator("table tbody tr", { hasText: "E2E Contact" })
      .getByRole("link", { name: /submissions/i })
      .click();
    await page.waitForURL(/\/submissions$/);
    await expect(page.getByText(/no submissions yet/i)).toBeVisible();
  });

  test("forms no longer clutter the Library overview", async ({ page }) => {
    await page.goto("/cms-admin/library");
    await expect(page.locator(".rcms-app-content")).not.toContainText("E2E Contact");
    // The forms *type* is filtered out of the overview entirely.
    await expect(page.locator('.rcms-app-content a[href*="type=form"]')).toHaveCount(0);
  });
});
