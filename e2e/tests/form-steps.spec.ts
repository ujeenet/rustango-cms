import { test, expect, type Page } from "@playwright/test";
import { DatabaseSync } from "node:sqlite";

// Multi-step forms, driven through the builder preview — the real public
// renderer and runtime. Each case was a bug a visitor could hit:
// Enter sent the whole form from step 1, required choices were only
// checked by the server, a refusal wiped every answer, and the browser's
// Back button left the form.
const DB = process.env.E2E_DB ?? ".state/demo.db";
// Assigned by the database: a fixed high id would move the table's id
// counter into the API fixture's range, and the forms other specs create
// afterwards would land there.
let FORM_ID = 0;
const SLUG = "e2e-steps";

const choice = (value: string, label: string) => ({ value, label });
const field = (f: Record<string, unknown>) => ({ id: `f-${f.key}`, label: "", ...f });
const page = (id: string, label: string, fields: unknown[]) => ({
  id,
  label,
  sections: [{ id: `s-${id}`, title: "", rows: [{ id: `r-${id}`, columns: [{ id: `c-${id}`, width: 12, fields }] }] }],
});

const schema = {
  settings: { builtin_css: true },
  pages: [
    page("p1", "About you", [
      field({ key: "name", type: "text", label: "Your name", required: true }),
    ]),
    page("p2", "Your piece", [
      field({ key: "piece", type: "radio", label: "What would you like?", required: true, options: [choice("mug", "Mug"), choice("bowl", "Bowl")] }),
      field({ key: "glazes", type: "checkboxes", label: "Glazes", required: true, options: [choice("moon", "Moon blue"), choice("white", "Satin white")] }),
    ]),
  ],
};

function removeForm(db: DatabaseSync) {
  db.prepare("DELETE FROM cms_form_entry WHERE form_snippet_id IN (SELECT id FROM cms_snippet WHERE slug = ?)").run(SLUG);
  db.prepare("DELETE FROM cms_snippet WHERE slug = ?").run(SLUG);
}

test.beforeAll(() => {
  const db = new DatabaseSync(DB);
  removeForm(db);
  const res = db.prepare(
    `INSERT INTO cms_snippet (slug, title, type_name, body_markdown, data, folder_path)
     VALUES (?, 'E2E Steps', 'form', '', ?, '')`,
  ).run(SLUG, JSON.stringify(schema));
  FORM_ID = Number(res.lastInsertRowid);
  db.close();
});

// forms.spec expects an empty Forms list: leave none behind.
// The fixture seed deletes snippets with foreign keys on: the submissions
// these tests made go first.
test.afterAll(() => {
  const db = new DatabaseSync(DB);
  removeForm(db);
  db.close();
});

const previewUrl = () => `/cms-admin/forms/${FORM_ID}/preview`;
const visibleStep = (p: Page) =>
  p.locator("[data-rcms-page]").evaluateAll((els) => els.findIndex((e) => !(e as HTMLElement).hidden));

test("Enter on a step moves to the next one instead of sending", async ({ page: p }) => {
  const posts: string[] = [];
  p.on("request", (r) => { if (r.method() === "POST" && r.url().includes("/forms/submit/")) posts.push(r.url()); });
  await p.goto(previewUrl());
  await p.fill('[name="name"]', "Ana");
  await p.locator('[name="name"]').press("Enter");
  await expect.poll(() => visibleStep(p)).toBe(1);
  await expect(p.locator("[data-rcms-progress]")).toHaveText("Step 2 of 2");
  expect(posts).toEqual([]);
});

test("a required radio and checkbox group are checked before sending", async ({ page: p }) => {
  await p.goto(previewUrl());
  await p.fill('[name="name"]', "Ana");
  await p.locator("[data-rcms-next]").click();
  await p.locator("[data-rcms-submit]").click();
  // Still here: the browser refused, focusing the first unanswered choice.
  await expect(p).toHaveURL(new RegExp(`${previewUrl()}$`));
  await expect(p.locator('[name="piece"]').first()).toBeFocused();
  await p.locator('[name="piece"][value="bowl"]').check();
  await p.locator("[data-rcms-submit]").click();
  await expect(p).toHaveURL(new RegExp(`${previewUrl()}$`));
  expect(await p.locator('[name="glazes"]').first().evaluate((e) => (e as HTMLInputElement).validity.valid)).toBe(false);
});

test("steps are history entries, and a reload keeps the step and the answers", async ({ page: p }) => {
  await p.goto(previewUrl());
  await p.fill('[name="name"]', "Ana");
  await p.locator("[data-rcms-next]").click();
  await p.locator('[name="piece"][value="mug"]').check();
  await p.goBack();
  await expect.poll(() => visibleStep(p)).toBe(0);
  await expect(p).toHaveURL(new RegExp(`${previewUrl()}$`)); // still on the form
  await p.goForward();
  await expect.poll(() => visibleStep(p)).toBe(1);
  await p.reload();
  await expect.poll(() => visibleStep(p)).toBe(1);
  await expect(p.locator('[name="name"]')).toHaveValue("Ana");
  await expect(p.locator('[name="piece"][value="mug"]')).toBeChecked();
});

test("a refused submission returns to the question with the answers kept", async ({ page: p }) => {
  await p.goto(previewUrl());
  await p.fill('[name="name"]', "Ana");
  await p.locator("[data-rcms-next]").click();
  await p.locator('[name="glazes"][value="moon"]').check();
  // Take the browser check away so the server has to catch it.
  await p.evaluate(() => document.querySelectorAll<HTMLInputElement>('[name="piece"]').forEach((i) => { i.required = false; i.checked = false; }));
  await p.locator("[data-rcms-submit]").click();
  await expect(p).toHaveURL(new RegExp(`form_error=${FORM_ID}:piece`));
  await expect(p.locator(".rcms-form-error")).toBeVisible();
  await expect.poll(() => visibleStep(p)).toBe(1);
  await expect(p.locator('.rcms-field-invalid[data-rcms-key="piece"]')).toHaveCount(1);
  await expect(p.locator('[name="name"]')).toHaveValue("Ana");
  await expect(p.locator('[name="glazes"][value="moon"]')).toBeChecked();

  await p.locator('[name="piece"][value="mug"]').check();
  await expect(p.locator(".rcms-field-invalid")).toHaveCount(0);
  await p.locator("[data-rcms-submit]").click();
  await expect(p).toHaveURL(new RegExp(`form_submitted=${FORM_ID}`));
  await expect(p.locator(".rcms-form-thanks")).toBeVisible();
  // The kept answers are gone once sent.
  expect(await p.evaluate((k) => sessionStorage.getItem(k), `rcms-form:${FORM_ID}:`)).toBeNull();
});
