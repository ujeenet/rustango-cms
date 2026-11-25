import { test, expect, Page, Locator } from "@playwright/test";
import path from "node:path";
import fs from "node:fs";

const RED = path.join(__dirname, "..", "fixtures", "red.png");
const BLUE = path.join(__dirname, "..", "fixtures", "blue.png");

// Fire a paste or drop carrying an image file on the editor surface.
async function sendImageFile(ed: Locator, event: "paste" | "drop", file: string) {
  const b64 = fs.readFileSync(file).toString("base64");
  await surface(ed).evaluate(
    (el, { event, b64, name }) => {
      const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
      const dt = new DataTransfer();
      dt.items.add(new File([bytes], name, { type: "image/png" }));
      const r = el.getBoundingClientRect();
      const ev =
        event === "paste"
          ? new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true })
          : new DragEvent("drop", {
              dataTransfer: dt,
              bubbles: true,
              cancelable: true,
              clientX: r.left + 10,
              clientY: r.top + 10,
            });
      el.dispatchEvent(ev);
    },
    { event, b64, name: path.basename(file) },
  );
}

// Every <img> in `scope` has loaded real pixels.
async function imagesLoaded(scope: Locator) {
  await expect
    .poll(() =>
      scope.evaluateAll((imgs) =>
        imgs.every((i) => (i as HTMLImageElement).complete && (i as HTMLImageElement).naturalWidth > 0),
      ),
    )
    .toBe(true);
}

// The RichText editor (TipTap) on a `rich_text` stream block: every
// toolbar tool, links of each kind, tables, the save → reload round trip,
// the public render, sanitising, and the block lifecycle (duplicate,
// remove, move).
test.describe.configure({ mode: "serial" });

let editUrl = "";

const editors = (page: Page) => page.locator("[data-stream-block] .rcms-richtext");
const surface = (ed: Locator) => ed.locator(".ProseMirror");
const tool = (ed: Locator, title: string) =>
  ed.locator(`.rcms-richtext-toolbar button[title="${title}"]`);
const storedHtml = (ed: Locator) =>
  ed.locator("xpath=preceding-sibling::textarea[1]").inputValue();

async function addRichText(page: Page) {
  await page.locator('[data-stream-root] [data-action="open-picker"]').first().click();
  await page
    .locator(
      '[data-block-picker] [data-block-option][data-type="rich_text"] [data-action="pick-block"], [data-block-picker] [data-action="pick-block"][data-type="rich_text"]',
    )
    .first()
    .click();
  await expect(editors(page).last()).toBeVisible();
  return editors(page).last();
}

async function typeInto(ed: Locator, text: string) {
  await surface(ed).click();
  await surface(ed).pressSequentially(text);
}

async function selectAll(page: Page, ed: Locator) {
  // Select the current line from the keyboard. (A triple-click on text
  // that is already selected starts a drag in ProseMirror instead.)
  await surface(ed).locator("p, h2, h3, h4, li, pre, blockquote").first().click();
  await page.keyboard.press("End");
  await page.keyboard.press("Shift+Home");
  await expect
    .poll(() => page.evaluate(() => (window.getSelection() || "").toString().length))
    .toBeGreaterThan(0);
}

async function save(page: Page) {
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/\d+\/edit)/);
}

test("create a page with a stream body", async ({ page }) => {
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill("E2E Richtext");
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);
  await page.getByRole("link", { name: "E2E Richtext" }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  editUrl = new URL(page.url()).pathname;
});

test("a rich text block becomes an editor with a toolbar", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await expect(ed.locator(".rcms-richtext-toolbar")).toBeVisible();
  await expect(surface(ed)).toHaveAttribute("contenteditable", "true");
  // The textarea stays in the form, hidden, as the submitted value.
  await expect(ed.locator("xpath=preceding-sibling::textarea[1]")).toBeHidden();
  expect(await storedHtml(ed)).toBe("");
});

test("inline marks and their active state", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "marked");
  for (const [title, tag] of [
    ["Bold", "strong"],
    ["Italic", "em"],
    ["Strikethrough", "s"],
    ["Inline code", "code"],
  ]) {
    await selectAll(page, ed);
    await tool(ed, title).click();
    await expect(tool(ed, title)).toHaveAttribute("aria-pressed", "true");
    expect(await storedHtml(ed)).toContain(`<${tag}>`);
    await tool(ed, title).click(); // toggle back off
    await expect(tool(ed, title)).toHaveAttribute("aria-pressed", "false");
  }
  // Keyboard shortcut
  await selectAll(page, ed);
  await page.keyboard.press("ControlOrMeta+b");
  expect(await storedHtml(ed)).toContain("<strong>marked</strong>");
});

test("block formats: headings, lists, quote, code block, divider", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "line");
  for (const [title, needle] of [
    ["Heading 2", "<h2>"],
    ["Heading 3", "<h3>"],
    ["Heading 4", "<h4>"],
    ["Bullet list", "<ul>"],
    ["Numbered list", "<ol>"],
    ["Quote", "<blockquote>"],
    ["Code block", "<pre>"],
  ]) {
    await tool(ed, title).click();
    expect(await storedHtml(ed), title).toContain(needle);
    await tool(ed, title).click(); // back to a paragraph
    expect(await storedHtml(ed), `${title} off`).not.toContain(needle);
  }
  await tool(ed, "Divider").click();
  expect(await storedHtml(ed)).toContain("<hr>");
});

test("links: external, email, anchor, remove; javascript: is refused", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "link me");
  const answer = (value: string) => page.once("dialog", (d) => d.accept(value));

  await selectAll(page, ed);
  answer("https://example.com/a");
  await tool(ed, "Insert / edit external link").click();
  expect(await storedHtml(ed)).toContain('href="https://example.com/a"');

  await selectAll(page, ed);
  answer("hello@example.com");
  await tool(ed, "Insert email link (mailto:)").click();
  expect(await storedHtml(ed)).toContain('href="mailto:hello@example.com"');

  await selectAll(page, ed);
  answer("#section-2");
  await tool(ed, "Link to an on-page anchor (#id)").click();
  expect(await storedHtml(ed)).toContain('href="#section-2"');

  await selectAll(page, ed);
  await tool(ed, "Remove link").click();
  expect(await storedHtml(ed)).not.toContain("<a ");

  await selectAll(page, ed);
  answer("javascript:alert(1)");
  await tool(ed, "Insert / edit external link").click();
  expect(await storedHtml(ed)).not.toContain("javascript:");
});

test("link to a page through the chooser", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "to the page");
  await selectAll(page, ed);
  await tool(ed, "Link to a page").click();
  const dialog = page.locator(".rcms-chooser-dialog");
  await expect(dialog).toBeVisible();
  await dialog.locator("[data-pc-results] [data-pc-row]").first().click();
  await expect(dialog).toBeHidden();
  const html = await storedHtml(ed);
  expect(html).toMatch(/linktype="page"/);
  expect(html).toMatch(/ id="\d+"/);
});

test("tables: insert, add and remove rows and columns, delete", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await surface(ed).click();
  await tool(ed, "Insert table").click();
  let html = await storedHtml(ed);
  expect(html).toContain("<table");
  expect((html.match(/<tr>/g) || []).length).toBe(3);
  await surface(ed).locator("td").first().click();
  await tool(ed, "Add row").click();
  expect(((await storedHtml(ed)).match(/<tr>/g) || []).length).toBe(4);
  await tool(ed, "Add column").click();
  await tool(ed, "Delete row").click();
  await tool(ed, "Delete column").click();
  expect(((await storedHtml(ed)).match(/<tr>/g) || []).length).toBe(3);
  await tool(ed, "Delete table").click();
  expect(await storedHtml(ed)).not.toContain("<table");
});

test("tools that cannot apply are disabled", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await surface(ed).click();
  for (const title of ["Add row", "Delete table", "Undo", "Redo", "Remove link"]) {
    await expect(tool(ed, title), title).toBeDisabled();
  }
  await tool(ed, "Insert table").click();
  await expect(tool(ed, "Add row")).toBeEnabled();
  await expect(tool(ed, "Delete table")).toBeEnabled();
  await expect(tool(ed, "Undo")).toBeEnabled();
});

test("a toolbar click leaves the caret in the editor", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "Title");
  await tool(ed, "Heading 2").click();
  // Enter must split the heading, not press the button again.
  await page.keyboard.press("Enter");
  await page.keyboard.type("next");
  await tool(ed, "Insert table").click();
  await page.keyboard.type("cell");
  const html = await storedHtml(ed);
  expect(html).toContain("<h2>Title</h2><p>next</p>");
  expect(html).toContain("<p>cell</p>");
});

test("insert an image from the media library", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await surface(ed).click();
  await tool(ed, "Insert image").click();
  const dialog = page.locator(".rcms-chooser-dialog");
  await expect(dialog).toBeVisible();
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    dialog.locator("[data-pc-upload]").click(),
  ]);
  await chooser.setFiles(RED);
  await expect(dialog).toBeHidden();
  await expect(surface(ed).locator("img")).toHaveCount(1);
  const html = await storedHtml(ed);
  const src = html.match(/<img src="([^"]+)"/)?.[1] || "";
  expect(src).toMatch(/^\/__media__\/raw\/\d+$/);
  expect(html).toContain('alt="red"');
  const res = await page.request.get(src);
  expect(res.status()).toBe(200);
  expect(res.headers()["content-type"]).toContain("image/png");
  await imagesLoaded(surface(ed).locator("img"));
});

test("a pasted or dropped image file is uploaded and inserted", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "pictures");
  await sendImageFile(ed, "paste", BLUE);
  await expect(surface(ed).locator("img")).toHaveCount(1);
  await sendImageFile(ed, "drop", RED);
  await expect(surface(ed).locator("img")).toHaveCount(2);
  const html = await storedHtml(ed);
  expect(html).not.toContain("data:");
  expect((html.match(/<img src="\/__media__\/raw\/\d+"/g) || []).length).toBe(2);
  await imagesLoaded(surface(ed).locator("img"));
});

test("undo and redo", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "abc");
  await selectAll(page, ed);
  await tool(ed, "Bold").click();
  expect(await storedHtml(ed)).toContain("<strong>");
  await tool(ed, "Undo").click();
  expect(await storedHtml(ed)).not.toContain("<strong>");
  await tool(ed, "Redo").click();
  expect(await storedHtml(ed)).toContain("<strong>");
});

test("content survives save and reload, and renders publicly", async ({ page }) => {
  await page.goto(editUrl);
  // Start from a clean stream: remove what earlier tests added.
  for (;;) {
    const remove = page.locator('[data-stream-block] [data-action="remove"]').first();
    if ((await remove.count()) === 0) break;
    await remove.click();
  }
  const ed = await addRichText(page);
  await typeInto(ed, "Saved heading");
  await tool(ed, "Heading 2").click();
  await page.keyboard.press("End");
  await page.keyboard.press("Enter");
  await page.keyboard.type("Body with ");
  await page.keyboard.press("ControlOrMeta+b");
  await page.keyboard.type("bold");
  await page.keyboard.press("ControlOrMeta+b");
  await page.keyboard.type(" text.");
  await page.keyboard.press("Enter");
  await tool(ed, "Insert table").click();
  await page.keyboard.type("cell");
  await sendImageFile(ed, "paste", RED);
  await expect(surface(ed).locator("img")).toHaveCount(1);
  const before = await storedHtml(ed);
  expect(before).toContain("<h2>Saved heading</h2>");
  expect(before).toContain("<p>cell</p>");
  await page.locator('select[name="status"]').selectOption("published");
  const slug = await page.locator('input[name="slug"]').inputValue();
  await save(page);

  await page.goto(editUrl);
  const again = editors(page).first();
  await expect(surface(again).locator("h2")).toHaveText("Saved heading");
  await expect(surface(again).locator("strong")).toHaveText("bold");
  await expect(surface(again).locator("table")).toHaveCount(1);
  await imagesLoaded(surface(again).locator("img"));
  // Loading the page does not rewrite the content.
  const after = await storedHtml(again);
  expect(after.replace(/\s+/g, "")).toBe(before.replace(/\s+/g, ""));

  // The public page renders the same content.
  const res = await page.request.get(`/${slug}/`);
  expect(res.status()).toBe(200);
  const live = await res.text();
  expect(live).toContain("<h2>Saved heading</h2>");
  expect(live).toContain("<strong>bold</strong>");
  expect(live).toMatch(/<table[\s\S]*<p>cell<\/p>/);
  const src = before.match(/<img src="([^"]+)"/)?.[1] || "";
  expect(live).toContain(`<img src="${src}"`);
  await page.goto(`/${slug}/`);
  await imagesLoaded(page.locator(`img[src="${src}"]`));
  // The starter site styles tables, so cells are visibly ruled.
  const border = await page.locator("article td").first().evaluate((td) => getComputedStyle(td).borderTopStyle);
  expect(border).toBe("solid");
});

test("pasted script is stripped on save", async ({ page }) => {
  await page.goto(editUrl);
  const ed = editors(page).first();
  await surface(ed).click();
  await page.evaluate(() => {
    const el = document.querySelector("[data-stream-block] .ProseMirror") as HTMLElement;
    const dt = new DataTransfer();
    dt.setData("text/html", '<p>pasted <a href="#" onclick="alert(1)">here</a><script>alert(2)</script>ok</p>');
    el.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
  });
  await save(page);
  await page.goto(editUrl);
  const html = await storedHtml(editors(page).first());
  expect(html).toContain("pasted");
  expect(html).not.toMatch(/onclick|<script|alert\(/);
});

test("duplicate keeps the content and gives an independent editor", async ({ page }) => {
  await page.goto(editUrl);
  const first = editors(page).first();
  const original = await storedHtml(first);
  await page.locator('[data-stream-block] [data-action="duplicate"]').first().click();
  await expect(editors(page)).toHaveCount(2);
  const copy = editors(page).nth(1);
  // The copy holds the same HTML, with no editor-only markup.
  const copied = await storedHtml(copy);
  expect(copied.replace(/\s+/g, "")).toBe(original.replace(/\s+/g, ""));
  expect(copied).not.toMatch(/ProseMirror|tableWrapper|<br class/);
  // Typing in the copy leaves the original alone.
  await surface(copy).locator("h2").click();
  await page.keyboard.press("End");
  await page.keyboard.type(" copy");
  expect(await storedHtml(copy)).toContain("Saved heading copy");
  expect(await storedHtml(first)).not.toContain("copy");
});

test("move and remove keep the editors working", async ({ page }) => {
  await page.goto(editUrl);
  await page.locator('[data-stream-block] [data-action="duplicate"]').first().click();
  await expect(editors(page)).toHaveCount(2);
  await page.locator('[data-stream-block] [data-action="move-down"]').first().click();
  const moved = editors(page).nth(1);
  await surface(moved).locator("h2").click();
  await page.keyboard.press("End");
  await page.keyboard.type("!");
  expect(await storedHtml(moved)).toContain("!");
  await page.locator('[data-stream-block] [data-action="remove"]').first().click();
  await expect(editors(page)).toHaveCount(1);
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await surface(editors(page).first()).click();
  await page.keyboard.type("x");
  expect(errors).toEqual([]);
});

test("an emptied editor stores an empty value", async ({ page }) => {
  await page.goto(editUrl);
  const ed = await addRichText(page);
  await typeInto(ed, "temporary");
  await selectAll(page, ed);
  await page.keyboard.press("Backspace");
  expect(await storedHtml(ed)).toBe("");
});
