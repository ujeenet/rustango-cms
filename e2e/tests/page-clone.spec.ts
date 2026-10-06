import { test, expect } from "@playwright/test";

// #742 — cloning copied only the core row: the copy opened with an empty
// body, no OG fields and no tags.
test("a cloned page carries the source's body, OG fields and tags", async ({ page }) => {
  const stamp = Date.now();
  const marker = `clone-marker-${stamp}`;
  const title = `Clone source ${stamp}`;

  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(title);
  await page.locator('input[name="slug"]').fill(`clone-src-${stamp}`);
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);

  await page.getByRole("link", { name: title }).first().click();
  await page.waitForURL(/\/cms-admin\/pages\/\d+\/edit/);
  const sourceId = page.url().match(/\/pages\/(\d+)\/edit/)![1];
  // Post the editor's own form with content set, bypassing the stream
  // editor's JS (which re-serializes the body from its DOM on submit).
  const saved = await page.evaluate(async ({ marker }) => {
    const form = document.getElementById("page-edit-form") as HTMLFormElement;
    const data = new FormData(form);
    data.set("body", JSON.stringify([{ type: "heading", id: "h1", value: { text: marker } }]));
    data.set("og_title", `OG ${marker}`);
    data.set("tags", marker);
    data.set("_action", "save");
    const res = await fetch(form.action, {
      method: "POST",
      body: new URLSearchParams(data as unknown as Record<string, string>),
      credentials: "same-origin",
      redirect: "manual",
    });
    return res.type === "opaqueredirect" ? 303 : res.status;
  }, { marker });
  expect(saved, "source saved").toBeLessThan(400);
  await page.reload();
  expect(await page.locator('[data-stream-name="body"] [data-stream-input]').inputValue()).toContain(marker);

  const csrf = (await page.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
  const res = await page.request.post(`/cms-admin/pages/${sourceId}/clone`, {
    form: { _csrf: csrf },
    headers: { "X-CSRF-Token": csrf },
    maxRedirects: 0,
  });
  expect(res.status(), await res.text()).toBeGreaterThanOrEqual(300);
  const editUrl = res.headers()["location"] ?? "";
  expect(editUrl).toMatch(/\/cms-admin\/pages\/\d+\/edit/);
  expect(editUrl).not.toContain(`/pages/${sourceId}/`);

  await page.goto(editUrl);
  await expect(page.locator('input[name="title"]')).toHaveValue(`${title} (copy)`);
  await expect(page.locator('[name="og_title"]')).toHaveValue(`OG ${marker}`);
  expect(await page.locator('[name="tags"]').inputValue()).toContain(marker);
  expect(await page.locator('[data-stream-name="body"] [data-stream-input]').inputValue()).toContain(marker);
  await expect(page.locator('select[name="status"]')).toHaveValue("draft");
});
