import { test, expect } from "@playwright/test";
import { DatabaseSync } from "node:sqlite";

// #705 — a save whose content write fails must say so, and must not have
// announced a publish first. It used to commit the status, fire the
// publish hooks and log "published", then drop the failed body write and
// answer "Saved".
const DB = process.env.E2E_DB ?? ".state/demo.db";

test("a failed content write refuses the save and announces no publish", async ({ page }) => {
  const stamp = Date.now();
  const title = `Content failure ${stamp}`;
  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(title);
  await page.locator('input[name="slug"]').fill(`content-failure-${stamp}`);
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);
  await page.getByRole("link", { name: title }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  const id = Number(page.url().match(/\/pages\/(\d+)\/edit/)![1]);

  // Make this one page's extension row unwritable.
  const db = new DatabaseSync(DB);
  const trigger = `rcms_e2e_fail_ext_${id}`;
  for (const op of ["INSERT", "UPDATE"]) {
    db.exec(
      `CREATE TRIGGER IF NOT EXISTS ${trigger}_${op.toLowerCase()} BEFORE ${op} ON demo_sectioned_page
       WHEN NEW.page_id = ${id} BEGIN SELECT RAISE(ABORT, 'e2e: extension write refused'); END`,
    );
  }
  try {
    const status = await page.evaluate(async () => {
      const form = document.getElementById("page-edit-form") as HTMLFormElement;
      const data = new FormData(form);
      data.set("status", "published");
      data.set("body", JSON.stringify([{ type: "heading", id: "h", value: { text: "new body" } }]));
      data.set("_action", "save");
      const res = await fetch(form.action, {
        method: "POST",
        body: new URLSearchParams(data as unknown as Record<string, string>),
        credentials: "same-origin",
        redirect: "manual",
      });
      return res.type === "opaqueredirect" ? 303 : res.status;
    });
    expect(status, "the failed save is reported, not redirected as saved").toBe(500);

    const published = db
      .prepare(`SELECT COUNT(*) AS n FROM cms_page_log_entry WHERE page_id = ? AND action = 'publish'`)
      .get(id) as { n: number };
    expect(published.n, "no publish was logged for content that never landed").toBe(0);
  } finally {
    for (const op of ["insert", "update"]) db.exec(`DROP TRIGGER IF EXISTS ${trigger}_${op}`);
    db.close();
  }
});
