import { test, expect, Page } from "@playwright/test";

// The block-tree sidebar: outline of every stream block (incl. nested
// typed_table rows), red live-validation, tree-driven canvas collapse,
// click-to-scroll, and the submit assist for collapsed invalid blocks.
test.describe.configure({ mode: "serial" });

let editUrl = "";
const dialogTree = (page: Page) => page.locator(".rcms-sidebar-blocktree");

async function openTree(page: Page) {
  await page.goto(editUrl);
  await page.locator(".rcms-blocktree-toggle").click();
  await expect(dialogTree(page)).toBeVisible();
}

async function addBlock(page: Page, type: string) {
  await page.locator('[data-stream-root] [data-action="open-picker"]').first().click();
  await page.locator(`[data-block-picker] [data-block-option][data-type="${type}"] [data-action="pick-block"], [data-block-picker] [data-action="pick-block"][data-type="${type}"]`).first().click();
}

test("create a Sectioned page (stream body)", async ({ page }) => {
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill("E2E Sections");
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);
  await page.getByRole("link", { name: "E2E Sections" }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  editUrl = new URL(page.url()).pathname;
});

test("tree toggle appears only on rcms-stream-editor pages", async ({ page }) => {
  await page.goto(editUrl);
  await expect(page.locator(".rcms-blocktree-toggle")).toBeVisible();
  await page.goto("/cms-admin/dashboard");
  await expect(page.locator(".rcms-blocktree-toggle")).toHaveCount(0);
});

test("activating swaps the sidebar nav for the tree (and back)", async ({ page }) => {
  await openTree(page);
  await expect(page.locator("aside.rcms-sidebar")).toHaveClass(/sidebar--blocktree/);
  await expect(page.locator(".rcms-sidebar-nav")).toBeHidden();
  await expect(page.locator(".rcms-blocktree-body")).toContainText(/no blocks yet/i);
  await page.locator("[data-bt-back]").click();
  await expect(page.locator(".rcms-sidebar-nav")).toBeVisible();
  await expect(dialogTree(page)).toBeHidden();
});

test("added blocks appear in the tree, nested rows included", async ({ page }) => {
  await page.goto(editUrl);
  await addBlock(page, "heading");
  await addBlock(page, "quote");
  await addBlock(page, "typed_table");
  // Add one nested row inside the table (Repeat → mints a typed_table_row).
  await page.locator('.rcms-stream-nested [data-action="open-picker"]').first().click();
  await page.locator(".rcms-blocktree-toggle").click();
  const nodes = page.locator("[data-bt-node]");
  await expect(nodes).toHaveCount(4); // heading, quote, typed_table, nested row
  await expect(page.locator(".rcms-blocktree-group")).toContainText(/rows/i);
  // Save so later tests start from persisted content — fill EVERY empty
  // required block field first (the submit assist correctly blocks saves
  // with invalid blocks, nested row cells included).
  await page.locator("[data-bt-back]").click();
  for (const inp of await page
    .locator("[data-stream-block] input[required], [data-stream-block] textarea[required]")
    .all()) {
    if (!(await inp.inputValue())) await inp.fill("x");
  }
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/\d+\/edit)/);
});

test("clicking a tree node scrolls the block into view", async ({ page }) => {
  await openTree(page);
  const last = page.locator("[data-bt-node] [data-bt-jump]").last();
  await last.click();
  // The jump target flashes and ends up inside the viewport.
  const flashed = page.locator("[data-stream-block].rcms-side-panel-flash");
  await expect(flashed).toHaveCount(1);
  await expect
    .poll(async () =>
      flashed.first().evaluate((el) => {
        const r = el.getBoundingClientRect();
        return r.top >= 0 && r.bottom <= window.innerHeight + 5;
      })
    )
    .toBe(true);
});

test("tree fold collapses the canvas block; collapse/expand all", async ({ page }) => {
  await openTree(page);
  await page.locator("[data-bt-node] [data-bt-fold]").first().click();
  await expect(page.locator("[data-stream-block][data-collapsed]")).toHaveCount(1);
  await expect(page.locator("[data-bt-node].is-collapsed")).toHaveCount(1);
  await page.locator("[data-bt-collapse-all]").click();
  const total = await page.locator("[data-stream-block]:not(template [data-stream-block])").count();
  await expect(page.locator("[data-stream-block][data-collapsed]")).toHaveCount(total);
  await page.locator("[data-bt-expand-all]").click();
  await expect(page.locator("[data-stream-block][data-collapsed]")).toHaveCount(0);
});

test("live validation: empty required field turns the node red", async ({ page }) => {
  await openTree(page);
  await expect(page.locator("[data-bt-node].is-invalid")).toHaveCount(0);
  const headingInput = page.locator('[data-stream-block][data-type="heading"] input[type="text"]').first();
  await headingInput.fill("");
  await expect(page.locator("[data-bt-node].is-invalid")).toHaveCount(1);
  await expect(page.locator("[data-bt-node].is-invalid .rcms-blocktree-errors")).toHaveText("1");
  await expect(page.locator("[data-stream-block].rcms-stream-block--invalid")).toHaveCount(1);
  await headingInput.fill("Hello again");
  await expect(page.locator("[data-bt-node].is-invalid")).toHaveCount(0);
});

test("submit assist expands a collapsed invalid block instead of failing silently", async ({ page }) => {
  await openTree(page);
  const headingInput = page.locator('[data-stream-block][data-type="heading"] input[type="text"]').first();
  await headingInput.fill("");
  // Collapse the now-invalid block, then try to save.
  await page.locator("[data-bt-node].is-invalid [data-bt-fold]").click();
  await expect(page.locator("[data-stream-block][data-collapsed]")).toHaveCount(1);
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  // The assist expands the block (no navigation happens).
  await expect(page.locator("[data-stream-block][data-collapsed]")).toHaveCount(0);
  await expect(page).toHaveURL(new RegExp(editUrl.replace(/\//g, "\\/")));
  // Restore validity so the fixture page stays saveable.
  await headingInput.fill("Hello");
});
