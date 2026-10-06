import { test, expect, Page } from "@playwright/test";
import * as path from "path";

const RED = path.resolve(__dirname, "../fixtures/red.png");
const BLUE = path.resolve(__dirname, "../fixtures/blue.png");

// The picker lives in the page editor (HomePage's hero field is a
// MediaPicker), so the suite first creates a Home page, then drives the
// full picker experience against it. Serial: later tests build on media
// uploaded by earlier ones.
test.describe.configure({ mode: "serial" });

let editUrl = "";

// The demo page types carry no media field of their own, so the suite
// drives the picker through the one every page has: the OG image on the
// Promote tab (also the hand-rolled markup that was converted to the
// standard picker). Helpers open that tab first.
function mediaWrapper(page: Page) {
  return page.locator('[data-chooser][data-chooser-kind="media"]').locator("visible=true").first();
}

async function openPromoteTab(page: Page) {
  await page.goto(editUrl);
  await page.getByRole("tab", { name: /promote/i }).click();
  await expect(mediaWrapper(page)).toBeVisible();
}

async function openPicker(page: Page) {
  await openPromoteTab(page);
  await mediaWrapper(page).locator("[data-chooser-open]").click();
  const dialog = page.locator(".rcms-chooser-dialog");
  await expect(dialog).toBeVisible();
  return dialog;
}

test("create a Home page to host the picker", async ({ page }) => {
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  // Page-type picker → the demo's Home page type.
  await page.getByRole("link", { name: /home page/i }).first().click();
  await page.locator('input[name="title"]').fill("E2E Home");
  // The page editor's Save (NOT .first() button[type=submit] — that's the
  // sidebar Sign-out form in DOM order).
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages/);
  // Open it for editing and remember the URL for the other tests.
  await page.getByRole("link", { name: "E2E Home" }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  editUrl = new URL(page.url()).pathname;
  await page.getByRole("tab", { name: /promote/i }).click();
  await expect(mediaWrapper(page)).toBeVisible();
});

test("rcms-empty library shows the upload-first rcms-empty state", async ({ page }) => {
  const dialog = await openPicker(page);
  await expect(dialog.locator("[data-pc-heading]")).toHaveText(/choose an image/i);
  // Media toolbar visible; plain-list results hidden.
  await expect(dialog.locator("[data-pc-media-tools]")).toBeVisible();
  await expect(dialog.locator("[data-pc-results]")).toBeHidden();
  await expect(dialog.locator(".rcms-picker__empty")).toContainText(/upload your first image/i);
});

test("inline upload auto-selects and fills the widget", async ({ page }) => {
  const dialog = await openPicker(page);
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    dialog.locator("[data-pc-upload]").click(),
  ]);
  await chooser.setFiles(RED);
  // Single-pick: the upload commits, auto-picks, and closes the dialog.
  await expect(dialog).toBeHidden();
  const wrapper = mediaWrapper(page);
  await expect(wrapper.locator("[data-chooser-label]")).toHaveText("red");
  await expect(wrapper.locator("[data-chooser-input]")).toHaveValue(/\d+/);
  await expect(wrapper.locator("[data-chooser-preview]")).toBeVisible();
});

test("picked image survives a save round-trip (widget contract)", async ({ page }) => {
  // Pick in the dialog, save the page, reopen — the id must persist.
  const dialog = await openPicker(page);
  await dialog.locator("[data-pc-item]").first().click();
  await dialog.locator("[data-pc-choose]").click();
  await expect(dialog).toBeHidden();
  const before = await mediaWrapper(page).locator("[data-chooser-input]").inputValue();
  expect(before).toMatch(/\d+/);
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/\d+\/edit)/);
  await openPromoteTab(page);
  await expect(mediaWrapper(page).locator("[data-chooser-input]")).toHaveValue(before);
  // The label hydrates to the media TITLE on load — never the raw "#id".
  await expect(mediaWrapper(page).locator("[data-chooser-label]")).toHaveText("red");
});

test("grid renders rendition thumbs; selection fills the detail pane", async ({ page }) => {
  const dialog = await openPicker(page);
  const card = dialog.locator("[data-pc-item]").first();
  await expect(card).toBeVisible();
  // Thumbnails come from the server-signed rendition endpoint, never raw.
  const src = await card.locator("img").getAttribute("src");
  expect(src).toContain("/__media__/fill-320x320");
  expect(src).toContain("v=");
  // …and actually load (catches a host that forgot rendition_route::router()).
  await expect
    .poll(async () => card.locator("img").evaluate((el: HTMLImageElement) => el.naturalWidth))
    .toBeGreaterThan(0);
  await card.click();
  const detail = dialog.locator("[data-pc-detail]");
  await expect(detail).toContainText("red.png");
  await expect(detail).toContainText("8 × 8");
  await expect(dialog.locator("[data-pc-choose]")).toBeEnabled();
});

test("search narrows and misses show an rcms-empty state", async ({ page }) => {
  const dialog = await openPicker(page);
  const search = dialog.locator("[data-pc-filter]");
  await search.fill("red");
  await expect(dialog.locator("[data-pc-item]")).toHaveCount(1);
  await search.fill("no-such-image");
  await expect(dialog.locator(".rcms-picker__empty")).toContainText(/no matches/i);
  await search.fill("");
  await expect(dialog.locator("[data-pc-item]")).toHaveCount(1);
});

test("list/grid toggle persists across reopen", async ({ page }) => {
  let dialog = await openPicker(page);
  await dialog.locator('[data-pc-view="list"]').click();
  await expect(dialog.locator("[data-pc-media-results]")).toHaveClass(/rcms-picker__list/);
  await page.keyboard.press("Escape");
  dialog = await openPicker(page);
  await expect(dialog.locator("[data-pc-media-results]")).toHaveClass(/rcms-picker__list/);
  await expect(dialog.locator('[data-pc-view="list"]')).toHaveAttribute("aria-pressed", "true");
  await dialog.locator('[data-pc-view="grid"]').click();
  await page.keyboard.press("Escape");
});

test("duplicate upload dedups to the existing file", async ({ page }) => {
  const dialog = await openPicker(page);
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    dialog.locator("[data-pc-upload]").click(),
  ]);
  await chooser.setFiles(RED);
  await expect(dialog).toBeHidden();
  // Still exactly one media row — reopen and count.
  const dialog2 = await openPicker(page);
  await expect(dialog2.locator("[data-pc-item]")).toHaveCount(1);
  await page.keyboard.press("Escape");
});

test("second upload appears in the grid alongside the first", async ({ page }) => {
  const dialog = await openPicker(page);
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    dialog.locator("[data-pc-upload]").click(),
  ]);
  await chooser.setFiles(BLUE);
  await expect(dialog).toBeHidden();
  const dialog2 = await openPicker(page);
  await expect(dialog2.locator("[data-pc-item]")).toHaveCount(2);
  // Newest first.
  await expect(dialog2.locator("[data-pc-item]").first()).toHaveAttribute("data-title", "blue");
  await page.keyboard.press("Escape");
});

test("explicit Choose applies the selected item", async ({ page }) => {
  const dialog = await openPicker(page);
  await dialog.locator('[data-pc-item][data-title="blue"]').click();
  await dialog.locator("[data-pc-choose]").click();
  await expect(dialog).toBeHidden();
  const wrapper = mediaWrapper(page);
  await expect(wrapper.locator("[data-chooser-label]")).toHaveText("blue");
});

test("clear button empties the widget", async ({ page }) => {
  await openPromoteTab(page);
  const wrapper = mediaWrapper(page);
  await expect(wrapper.locator("[data-chooser-input]")).toHaveValue(/\d+/);
  await wrapper.locator("[data-chooser-clear]").click();
  await expect(wrapper.locator("[data-chooser-input]")).toHaveValue("");
});
