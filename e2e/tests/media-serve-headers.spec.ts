import { test, expect, type Page } from "@playwright/test";

// #724 — media shares the tenant host with /cms-admin, so a file a browser
// would run as a page must never render there.

async function upload(page: Page, name: string, mimeType: string, body: string, title: string) {
  await page.goto("/cms-admin/media/upload");
  const csrf = (await page.context().cookies()).find((c) => c.name === "rustango_csrf")?.value ?? "";
  return page.request.post("/cms-admin/media/upload", {
    headers: { "X-CSRF-Token": csrf },
    multipart: { _csrf: csrf, title, file: { name, mimeType, buffer: Buffer.from(body) } },
  });
}

test("an HTML file is refused at upload", async ({ page }) => {
  const title = `upload-html-${Date.now()}`;
  const up = await upload(page, "x.html", "text/html", "<script>window.__ran=1</script>", title);
  expect(up.status(), await up.text()).toBeGreaterThanOrEqual(400);
  await page.goto("/cms-admin/documents");
  await expect(page.locator("tr", { hasText: title })).toHaveCount(0);
});

test("a non-viewable upload downloads sandboxed instead of rendering", async ({ page }) => {
  const title = `upload-zip-${Date.now()}`;
  const up = await upload(page, "x.zip", "application/zip", "PK", title);
  expect(up.status(), await up.text()).toBeLessThan(400);

  await page.goto("/cms-admin/documents");
  const action = await page
    .locator("tr", { hasText: title })
    .locator('form[action*="/delete"]')
    .first()
    .getAttribute("action");
  const id = action?.match(/\/(\d+)\//)?.[1];
  expect(id, `no media row linked for ${title}`).toBeTruthy();

  const res = await page.request.get(`/__media__/raw/${id}`, { maxRedirects: 0 });
  expect(res.status()).toBe(200);
  const h = res.headers();
  expect(h["content-type"]).toBe("application/octet-stream");
  expect(h["content-disposition"] ?? "").toMatch(/^attachment;/);
  expect(h["content-security-policy"]).toBe("sandbox");
  expect(h["x-content-type-options"]).toBe("nosniff");
});
