import { test, expect } from "@playwright/test";

// #696 — bundles loaded from script are served immutable, so their URL
// must carry the build token, or an upgrade keeps the old one for a year.
test("the TipTap bundle loads with the admin build token", async ({ page }) => {
  await page.goto("/cms-admin/pages");
  const loader = await page.locator('script[src*="/cms-admin/static/richtext-editor.js"]').getAttribute("src");
  const token = new URL(loader!, page.url()).search;
  expect(token, "the admin loads the loader versioned").toMatch(/^\?v=/);

  const bundle = page.waitForRequest((r) => r.url().includes("/cms-admin/static/vendor/tiptap.bundle.js"));
  await page.evaluate(() => {
    const ta = document.createElement("textarea");
    ta.setAttribute("data-widget-mode", "richtext");
    ta.name = "probe";
    document.body.appendChild(ta);
    // The hook the stream editor calls for fields it adds after load.
    (window as unknown as { rcmsEnhanceRichtext: (root?: Element) => void }).rcmsEnhanceRichtext();
  });
  const req = await bundle;
  expect(new URL(req.url()).search).toBe(token);
});
