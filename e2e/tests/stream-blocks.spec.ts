import { test, expect, Page, Locator } from "@playwright/test";
import path from "node:path";

// Every built-in block and the markdown editor: fill each one, save,
// reload, publish, and check what the public page renders.
test.describe.configure({ mode: "serial" });

const RED = path.join(__dirname, "..", "fixtures", "red.png");
const TITLE = `E2E Blocks ${Date.now()}`;
let editUrl = "";
let slug = "";

const root = (page: Page) => page.locator('[data-stream-root][data-stream-name="body"]');
const blockOf = (page: Page, type: string) =>
  root(page).locator(`:scope [data-stream-block][data-type="${type}"]`);
const field = (blk: Locator, name: string) => blk.locator(`[name$="__${name}"]`).first();

async function addBlock(page: Page, type: string) {
  const r = root(page);
  const dialog = r.locator(":scope > dialog[data-block-picker]");
  await r.locator(':scope > .rcms-stream-picker [data-action="open-picker"]').click();
  await dialog.locator("[data-block-picker-search]").fill(type);
  await dialog.locator(`[data-action="pick-block"][data-type="${type}"]`).click();
  return blockOf(page, type).last();
}

// Add one item to a block's repeat list (cards, slides, rows, pictures).
async function addItem(blk: Locator, itemType: string) {
  const list = blk.locator(`[data-stream-list][data-allowed="${itemType}"]`).first();
  const before = await list.locator(`:scope > [data-stream-block]`).count();
  await blk.locator(`[data-action="open-picker"]`).first().click();
  await expect(list.locator(`:scope > [data-stream-block]`)).toHaveCount(before + 1);
  return list.locator(`:scope > [data-stream-block]`).last();
}

// Pick the uploaded image in a media chooser inside `scope`.
async function pickImage(page: Page, scope: Locator, upload = false) {
  await scope.locator("[data-chooser-open]").first().click();
  const dialog = page.locator(".rcms-chooser-dialog");
  await expect(dialog).toBeVisible();
  if (upload) {
    const [chooser] = await Promise.all([
      page.waitForEvent("filechooser"),
      dialog.locator("[data-pc-upload]").click(),
    ]);
    await chooser.setFiles(RED);
  } else {
    await dialog.locator("[data-pc-item]").first().click();
    await dialog.locator("[data-pc-choose]").click();
  }
  await expect(dialog).toBeHidden();
}

async function saveAndStay(page: Page) {
  // The URL is the same before and after, so wait for the save itself.
  await Promise.all([
    page.waitForResponse((r) => r.request().method() === "POST" && /\/cms-admin\/pages\/\d+\/edit/.test(r.url())),
    page.locator('button[form="page-edit-form"][value="continue"]').click(),
  ]);
  await page.waitForLoadState("load");
}

test("create a page with every block", async ({ page }) => {
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(TITLE);
  await page.locator('select[name="status"]').selectOption("published");
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);
  await page.getByRole("link", { name: TITLE }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  editUrl = new URL(page.url()).pathname;
  slug = await page.locator('input[name="slug"]').inputValue();
});

test("fill every block, save and render", async ({ page }) => {
  test.setTimeout(60000);
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(editUrl);

  let b = await addBlock(page, "heading");
  await field(b, "text").fill("Heading block");
  b = await addBlock(page, "paragraph");
  await field(b, "body").fill("Paragraph with **md bold** and [a link](https://example.com/md).");
  b = await addBlock(page, "rich_text");
  await b.locator(".ProseMirror").click();
  await b.locator(".ProseMirror").pressSequentially("Rich text block");
  b = await addBlock(page, "quote");
  await field(b, "text").fill("Quoted words");
  await field(b, "attribution").fill("Someone");
  b = await addBlock(page, "image");
  await pickImage(page, b, true);
  await field(b, "alt").fill("A red square");
  await field(b, "caption").fill("Image caption");
  b = await addBlock(page, "embed");
  await field(b, "url").fill("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
  b = await addBlock(page, "code");
  await field(b, "code").fill("fn main() { println!(\"hi\"); }");
  b = await addBlock(page, "table");
  await field(b, "tsv").fill("Name\tQty\nApple\t3\nPear\t5");
  await field(b, "caption").fill("Fruit table");
  b = await addBlock(page, "typed_table");
  await field(b, "caption").fill("Typed caption");
  let item = await addItem(b, "typed_table_row");
  await field(item, "label").fill("Row label");
  await field(item, "value").fill("7");
  await field(item, "notes").fill("Row notes");
  b = await addBlock(page, "raw_html");
  await field(b, "html").fill('<p class="raw-check">Raw HTML ok</p><script>alert(1)</script>');
  b = await addBlock(page, "cards");
  await field(b, "heading").fill("Cards heading");
  item = await addItem(b, "card");
  await field(item, "title").fill("Card one");
  await field(item, "text").fill("Card text");
  await pickImage(page, item);
  b = await addBlock(page, "slider");
  await field(b, "heading").fill("Slider heading");
  item = await addItem(b, "slide");
  await field(item, "heading").fill("Slide one");
  await field(item, "alt").fill("Slide image");
  await pickImage(page, item);
  b = await addBlock(page, "gallery");
  await field(b, "caption").fill("Gallery caption");
  item = await addItem(b, "gallery_image");
  await field(item, "alt").fill("Gallery picture");
  await pickImage(page, item);
  b = await addBlock(page, "text");
  await field(b, "body").fill("Plain text block");
  b = await addBlock(page, "email");
  await field(b, "value").fill("hello@example.com");
  await field(b, "label").fill("Email us");
  b = await addBlock(page, "url");
  await field(b, "value").fill("https://example.com/url-block");
  await field(b, "label").fill("URL label");
  b = await addBlock(page, "integer");
  await field(b, "value").fill("42");
  b = await addBlock(page, "float");
  await field(b, "value").fill("3.25");
  b = await addBlock(page, "decimal");
  await field(b, "value").fill("19.99");
  b = await addBlock(page, "boolean");
  await field(b, "value").check();
  b = await addBlock(page, "date");
  await field(b, "value").fill("2026-11-24");
  b = await addBlock(page, "time");
  await field(b, "value").fill("13:45");
  b = await addBlock(page, "datetime");
  await field(b, "value").fill("2026-11-24T13:45");
  b = await addBlock(page, "choice");
  b = await addBlock(page, "multiple_choice");
  b = await addBlock(page, "regex");
  await field(b, "value").fill("ABC-123");
  b = await addBlock(page, "static");
  b = await addBlock(page, "page_chooser");
  await b.locator("[data-chooser-open]").click();
  await page.locator(".rcms-chooser-dialog [data-pc-results] [data-pc-row]").first().click();
  await field(b, "label").fill("Page link label");
  b = await addBlock(page, "snippet_chooser");
  b = await addBlock(page, "document_chooser");
  b = await addBlock(page, "form");

  await saveAndStay(page);
  expect(errors).toEqual([]);

  // Every block came back with its values.
  const stored = JSON.parse(await root(page).locator("[data-stream-input]").inputValue());
  const byType = Object.fromEntries(stored.map((b: any) => [b.type, b.value]));
  expect(stored).toHaveLength(31);
  expect(byType.heading).toMatchObject({ text: "Heading block", level: "2" });
  expect(byType.paragraph.body).toContain("**md bold**");
  expect(byType.rich_text.body).toBe("<p>Rich text block</p>");
  expect(byType.quote).toMatchObject({ text: "Quoted words", attribution: "Someone" });
  expect(byType.image.media_id).toMatch(/^\d+$/);
  expect(byType.typed_table.rows[0].value).toMatchObject({ label: "Row label", notes: "Row notes" });
  expect(byType.cards.cards[0].value.title).toBe("Card one");
  expect(byType.slider.slides[0].value.heading).toBe("Slide one");
  expect(byType.gallery.images[0].value.alt).toBe("Gallery picture");
  expect(byType.integer.value).toBe("42");
  expect(byType.boolean.value).toBeTruthy();
  expect(byType.datetime.value).toBe("2026-11-24T13:45");
  expect(byType.page_chooser.page_id).toMatch(/^\d+$/);

  // The public page renders each one.
  const res = await page.request.get(`/${slug}/`);
  expect(res.status()).toBe(200);
  const html = await res.text();
  for (const needle of [
    /<h2 class="rcms-block-heading">Heading block<\/h2>/,
    /<strong>md bold<\/strong> and <a href="https:\/\/example.com\/md"/,
    /<div class="rcms-block-rich-text"><p>Rich text block<\/p><\/div>/,
    /<blockquote class="rcms-block-quote">[\s\S]*Quoted words[\s\S]*— Someone/,
    /<figure class="rcms-block-image"><img src="\/__media__\/[^"]+"\s+alt="A red square"[\s\S]*Image caption/,
    /<iframe src="https:\/\/www.youtube.com\/embed\/dQw4w9WgXcQ"/,
    /<pre class="rcms-block-code"><code>fn main\(\)/,
    /<th>Name<\/th><th>Qty<\/th>[\s\S]*<td>Pear<\/td><td>5<\/td>[\s\S]*Fruit table/,
    /<td>Row label<\/td>[\s\S]*Typed caption/,
    /<div class="rcms-block-raw-html"><p>Raw HTML ok<\/p><\/div>/,
    /rcms-card__title">Card one</,
    /rcms-slide__heading">Slide one</,
    /alt="Gallery picture"[\s\S]*Gallery caption/,
    /Plain text block/,
    /href="mailto:hello@example.com">Email us</,
    />URL label</,
    /rcms-block-integer">42</,
    /rcms-block-float">3.25</,
    /rcms-block-decimal">19.99</,
    /rcms-block-boolean--on/,
    /datetime="2026-11-24">/,
    /datetime="13:45">/,
    /rcms-block-regex">ABC-123</,
    /<hr class="rcms-block-static">/,
    /rcms-block-page-chooser"[^>]*>Page link label</,
  ]) {
    expect(html, String(needle)).toMatch(needle);
  }
  expect(html).not.toContain("alert(1)");
  // Blocks the editor left empty render nothing, not an error.
  expect(html).not.toContain("rcms-form-missing");

  // Every image on the public page loads.
  await page.goto(`/${slug}/`);
  const imgs = page.locator("article img");
  await expect(imgs).toHaveCount(4);
  await expect
    .poll(() => imgs.evaluateAll((all) => all.every((i) => (i as HTMLImageElement).naturalWidth > 0)))
    .toBe(true);
});

// The markdown editor (Paragraph block): toolbar, shortcuts, and that
// each block's toolbar edits its own textarea.
test.describe("markdown editor", () => {
  const md = (blk: Locator) => blk.locator('textarea[data-widget-mode="markdown"]');
  const btn = (blk: Locator, action: string) => blk.locator(`[data-md-action="${action}"]`);

  async function freshParagraphs(page: Page, n: number) {
    await page.goto(editUrl);
    for (;;) {
      const remove = root(page).locator('[data-stream-block] [data-action="remove"]').first();
      if ((await remove.count()) === 0) break;
      await remove.click();
    }
    for (let i = 0; i < n; i++) await addBlock(page, "paragraph");
    return blockOf(page, "paragraph");
  }

  async function selectLine(blk: Locator, text: string) {
    await md(blk).fill(text);
    await md(blk).evaluate((t: HTMLTextAreaElement) => t.setSelectionRange(0, t.value.length));
  }

  test("every tool, on the block it belongs to", async ({ page }) => {
    const blocks = await freshParagraphs(page, 2);
    const first = blocks.nth(0);
    const second = blocks.nth(1);
    await expect(first.locator(".rcms-md-toolbar")).toBeVisible();
    await expect(second.locator(".rcms-md-toolbar")).toBeVisible();
    await md(first).fill("untouched");

    for (const [action, input, out] of [
      ["bold", "word", "**word**"],
      ["italic", "word", "_word_"],
      ["h2", "Title", "## Title"],
      ["h3", "Title", "### Title"],
      ["ul", "a\nb", "- a\n- b"],
      ["ol", "a\nb", "1. a\n2. b"],
      ["quote", "said", "> said"],
      ["code", "x", "`x`"],
      ["codeblock", "x", "\n```\nx\n```\n"],
    ]) {
      await selectLine(second, input);
      await btn(second, action).click();
      expect(await md(second).inputValue(), action).toBe(out);
    }
    page.once("dialog", (d) => d.accept("https://example.com/x"));
    await selectLine(second, "site");
    await btn(second, "link").click();
    expect(await md(second).inputValue()).toBe("[site](https://example.com/x)");
    // The first block was never touched.
    expect(await md(first).inputValue()).toBe("untouched");
  });

  test("keyboard shortcuts", async ({ page }) => {
    const blk = (await freshParagraphs(page, 1)).first();
    await selectLine(blk, "word");
    await md(blk).press("ControlOrMeta+b");
    expect(await md(blk).inputValue()).toBe("**word**");
    await selectLine(blk, "word");
    await md(blk).press("ControlOrMeta+i");
    expect(await md(blk).inputValue()).toBe("_word_");
    page.once("dialog", (d) => d.accept("https://example.com/k"));
    await selectLine(blk, "word");
    await md(blk).press("ControlOrMeta+k");
    expect(await md(blk).inputValue()).toBe("[word](https://example.com/k)");
  });

  test("saved and duplicated blocks keep a working toolbar", async ({ page }) => {
    const blk = (await freshParagraphs(page, 1)).first();
    await md(blk).fill("saved");
    await saveAndStay(page);
    const saved = blockOf(page, "paragraph").first();
    await expect(saved.locator(".rcms-md-toolbar")).toBeVisible();
    await saved.locator('[data-action="duplicate"]').first().click();
    const copy = blockOf(page, "paragraph").nth(1);
    await expect(copy.locator(".rcms-md-toolbar")).toBeVisible();
    await selectLine(copy, "copy");
    await btn(copy, "bold").click();
    expect(await md(copy).inputValue()).toBe("**copy**");
    expect(await md(saved).inputValue()).toBe("saved");
    await selectLine(saved, "first");
    await btn(saved, "italic").click();
    expect(await md(saved).inputValue()).toBe("_first_");
    expect(await md(copy).inputValue()).toBe("**copy**");
  });
});
