import { test, expect, Page } from "@playwright/test";

// The form translation screen uses the page editor's layout: the original
// text left, the translation right, grouped per field. Saving comes back to
// that screen. (Snippet translation mode is a template test in
// tests/admin_surface.rs — the demo registers no library type.)
// Uses its own locale (`de`) so the API suite's fixed-id `fr` is untouched;
// the last test deletes it again — with its translations, which the foreign
// keys used to refuse.
test.describe.configure({ mode: "serial" });

const CODE = "de";

async function localeRow(page: Page) {
  await page.goto("/cms-admin/locales");
  return page.locator("table tbody tr", { has: page.locator("td", { hasText: new RegExp(`^${CODE}$`) }) });
}

test.describe("translation screens", () => {
  test.beforeAll(async ({ browser }) => {
    const page = await browser.newPage({ storageState: ".state/admin.json" });
    if ((await (await localeRow(page)).count()) === 0) {
      await page.goto("/cms-admin/locales/new");
      await page.locator('input[name="code"]').fill(CODE);
      await page.locator('input[name="name"]').fill("Deutsch");
      await page.locator('.rcms-app-content button[type="submit"]').first().click();
      await page.waitForURL(/\/cms-admin\/locales$/);
    }
    await page.close();
  });

  test("a form's texts are grouped, and saving returns to the translation", async ({ page }) => {
    await page.goto("/cms-admin/forms");
    await page.getByRole("link", { name: /new form/i }).first().click();
    await page.locator('input[name="title"]').fill("E2E Translated form");
    await page.locator('.rcms-app-content button[type="submit"]').first().click();
    await page.waitForURL(/\/cms-admin\/forms\/\d+\/build/);
    const build = page.url().split("?")[0];

    // Give the form a text to translate, then publish (translations read
    // the published form).
    await page.locator("#fb-settings-toggle").click();
    await page.locator("#fb-settings input[placeholder='Submit']").fill("Send it");
    await page.locator("button[formaction]").click();
    await page.waitForURL(/\/build/);

    await page.goto(`${build}?locale=${CODE}`);
    const row = page.locator(".rcms-tr-row", { has: page.locator('input[name="tr__settings.submit_label"]') });
    await expect(row.locator(".rcms-canonical-text")).toHaveText("Send it");
    // A one-page form has no "page label" row: it is never shown.
    await expect(page.locator('[name^="tr__page."]')).toHaveCount(0);

    await row.locator("input").fill("Abschicken");
    await page.getByRole("button", { name: /save translations/i }).click();
    await page.waitForURL(new RegExp(`/build\\?locale=${CODE}$`));
    await expect(page.locator('input[name="tr__settings.submit_label"]')).toHaveValue("Abschicken");
  });

  test("a category's name is translated beside the original", async ({ page }) => {
    const name = `Mugs ${Date.now()}`;
    await page.goto("/cms-admin/taxonomies/category/categories/new");
    await page.locator('main input[name="name"]').fill(name);
    await page.locator("main form button", { hasText: "Create category" }).click();
    await page.waitForURL(/\/categories\/\d+\/edit/);
    const edit = page.url().split("?")[0];

    // The language buttons switch the form to translation mode.
    await page.locator(".rcms-app-topbar a", { hasText: new RegExp(`^${CODE}$`) }).click();
    await page.waitForURL(new RegExp(`\\?locale=${CODE}$`));
    await expect(page.locator('main input[name="name"]')).toHaveCount(0);
    await expect(page.locator(".rcms-tr-row .rcms-canonical-text")).toHaveText(name);
    await page.locator('input[name="tr__name"]').fill("Tassen");
    await page.getByRole("button", { name: /save translations/i }).click();
    await page.waitForURL(`${edit}?locale=${CODE}`);
    await expect(page.locator('input[name="tr__name"]')).toHaveValue("Tassen");
  });

  test("deleting a locale removes it together with its translations", async ({ page }) => {
    const row = await localeRow(page);
    await expect(row).toHaveCount(1);
    // Submit directly: the delete button opens a confirm dialog.
    await Promise.all([
      page.waitForURL(/\/cms-admin\/locales$/),
      row.locator("form").evaluate((f: HTMLFormElement) => f.submit()),
    ]);
    await expect(page.getByText(/deleted locale deutsch/i)).toBeVisible();
    await expect(await localeRow(page)).toHaveCount(0);
  });
});
