import { test, expect } from "@playwright/test";

// Regression net for the NON-media chooser kinds after the picker
// redesign: same dialog shell, plain row list, media toolbar hidden,
// and the programmatic rcmsOpenChooser contract (dismiss → null).
test.describe("shared chooser — non-media kinds", () => {
  test("page chooser keeps the plain list inside the new shell", async ({ page }) => {
    await page.goto("/cms-admin/dashboard");
    const resolved = page.evaluate(() => (window as any).rcmsOpenChooser("page"));
    const dialog = page.locator(".rcms-chooser-dialog");
    await expect(dialog).toBeVisible();
    await expect(dialog.locator("[data-pc-heading]")).toHaveText(/choose a page/i);
    await expect(dialog.locator("[data-pc-icon]")).toHaveText("article");
    // Media chrome stays off for other kinds.
    await expect(dialog.locator("[data-pc-media-tools]")).toBeHidden();
    await expect(dialog.locator("[data-pc-media-body]")).toBeHidden();
    await expect(dialog.locator("[data-pc-results]")).toBeVisible();
    await expect(dialog).not.toHaveClass(/rcms-chooser-dialog--media/);
    // Dismissing resolves the promise with null (richtext link flow).
    // The dialog focuses its filter; Esc from there must still close it
    // (a global Esc handler used to swallow it and only blur the field).
    await expect(dialog.locator("[data-pc-filter]")).toBeFocused();
    await page.keyboard.press("Escape");
    expect(await resolved).toBeNull();
  });

  test("document chooser opens with its own icon + empty state", async ({ page }) => {
    await page.goto("/cms-admin/dashboard");
    const resolved = page.evaluate(() => (window as any).rcmsOpenChooser("document"));
    const dialog = page.locator(".rcms-chooser-dialog");
    await expect(dialog).toBeVisible();
    await expect(dialog.locator("[data-pc-icon]")).toHaveText("description");
    await expect(dialog.locator(".rcms-picker__empty")).toContainText(/no documents/i);
    // The dialog focuses its filter; Esc from there must still close it
    // (a global Esc handler used to swallow it and only blur the field).
    await expect(dialog.locator("[data-pc-filter]")).toBeFocused();
    await page.keyboard.press("Escape");
    expect(await resolved).toBeNull();
  });
});
