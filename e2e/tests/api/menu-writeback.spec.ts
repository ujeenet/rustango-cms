/**
 * The editor round-trip: does saving a menu preserve what the API serves?
 *
 * Everything else in this suite reads. This spec writes — through the
 * real admin builder and the real save endpoint — and then re-reads the
 * API to see what survived. It is where the destructive bugs live.
 *
 * Runs as `@admin`, so it gets the admin session.
 */
import { APIRequestContext, expect, test } from "@playwright/test";
import { getJson, labels } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();

test.describe("@admin menu write-back", () => {
  /** The builder posts JSON with the CSRF token in a header. */
  async function csrf(api: APIRequestContext, menuId: number): Promise<string> {
    await api.get(`/cms-admin/navigation/${menuId}/edit`);
    const state = await api.storageState();
    const c = state.cookies.find((x) => x.name === "rustango_csrf");
    expect(c, "no CSRF cookie after loading the builder").toBeTruthy();
    return c!.value;
  }

  async function saveTree(api: APIRequestContext, menuId: number, items: unknown[]) {
    const token = await csrf(api, menuId);
    return api.post(`/cms-admin/navigation/${menuId}/save-tree`, {
      headers: { "x-csrf-token": token, "content-type": "application/json" },
      data: { items },
    });
  }

  /** The current tree as the API reports it — the thing that must not change. */
  const shape = (items: any[]): any =>
    items.map((i) => ({ id: i.id, label: i.label, children: shape(i.children ?? []) }));

  test("a duplicate item id is rejected, not a panic", async ({ request }) => {
    // Regression: the upsert used to `.expect()` a row it had already
    // claimed, panicking mid-transaction and killing the request.
    const res = await saveTree(request, f.menu.scratch, [
      { id: 999001, local_id: 0, parent_local_id: null, label: "one", page_id: f.page.home },
      { id: 999001, local_id: 1, parent_local_id: null, label: "two", page_id: f.page.home },
    ]);
    expect(res.status(), "a duplicate id must be a validation error").toBeLessThan(500);
    // And the menu must be untouched — still empty, not half-written.
    const after = await getJson(request, "/api/v2/menus/t-scratch/");
    expect(after.items).toEqual([]);
  });

  test("a translated item can be deleted", async ({ request }) => {
    // Regression: items were deleted before their translation rows, so
    // the FK refused and the whole save rolled back — a translated item
    // was undeletable.
    const before = await getJson(request, "/api/v2/menus/t-main/?locale=fr");
    expect(labels(before.items), "the fixture translated this item").toContain("T Accueil");

    // Keep everything except the translated "T Home link" item.
    const res = await saveTree(request, f.menu.main, [
      { id: f.item.docs, local_id: 0, parent_local_id: null, label: "T Docs link", page_id: f.page.docs },
      { id: f.item.introNoLabel, local_id: 1, parent_local_id: 0, label: "", page_id: f.page.intro },
    ]);
    expect(res.status(), await res.text()).toBeLessThan(400);

    const after = await getJson(request, "/api/v2/menus/t-main/?locale=fr");
    expect(labels(after.items)).not.toContain("T Accueil");
    expect(labels(after.items)).toContain("T Docs link");
  });

  test("a three-level subtree can be deleted in one save", async ({ request }) => {
    // Regression: removals were ordered by `parent_id.is_none()`, which
    // ties every non-root, so a grandchild could be deleted after its
    // parent and trip the self-FK nondeterministically.
    const build = await saveTree(request, f.menu.scratch, [
      { id: null, local_id: 0, parent_local_id: null, label: "L1", page_id: f.page.home },
      { id: null, local_id: 1, parent_local_id: 0, label: "L2", page_id: f.page.about },
      { id: null, local_id: 2, parent_local_id: 1, label: "L3", page_id: f.page.docs },
    ]);
    expect(build.status(), await build.text()).toBeLessThan(400);
    expect(labels((await getJson(request, "/api/v2/menus/t-scratch/")).items)).toEqual([
      "L1",
      "L2",
      "L3",
    ]);

    // Now remove all three at once.
    const wipe = await saveTree(request, f.menu.scratch, []);
    expect(wipe.status(), await wipe.text()).toBeLessThan(400);
    expect((await getJson(request, "/api/v2/menus/t-scratch/")).items).toEqual([]);
  });

  test("item ids survive a reorder, so translations do too", async ({ request }) => {
    // The whole reason the save diffs instead of drop-and-rebuild.
    const build = await saveTree(request, f.menu.scratch, [
      { id: null, local_id: 0, parent_local_id: null, label: "First", page_id: f.page.home },
      { id: null, local_id: 1, parent_local_id: null, label: "Second", page_id: f.page.about },
    ]);
    expect(build.status()).toBeLessThan(400);
    const before = (await getJson(request, "/api/v2/menus/t-scratch/")).items;
    const [first, second] = before;

    // Swap them, carrying the ids.
    const swap = await saveTree(request, f.menu.scratch, [
      { id: second.id, local_id: 0, parent_local_id: null, label: "Second", page_id: f.page.about },
      { id: first.id, local_id: 1, parent_local_id: null, label: "First", page_id: f.page.home },
    ]);
    expect(swap.status()).toBeLessThan(400);

    const after = (await getJson(request, "/api/v2/menus/t-scratch/")).items;
    expect(after.map((i: any) => i.label)).toEqual(["Second", "First"]);
    expect(after.map((i: any) => i.id), "ids must be stable across a reorder").toEqual([
      second.id,
      first.id,
    ]);
  });

  // -------------------------------------------------------------------

  test("saving to a menu that does not exist is a 404", async ({ request }) => {
    // Was a raw FK violation surfacing as a 500.
    const res = await saveTree(request, 999999, [
      { id: null, local_id: 0, parent_local_id: null, label: "orphan", page_id: f.page.home },
    ]);
    expect(res.status()).toBe(404);
  });

  test("opening the builder and pressing Save preserves the tree", async ({
    page,
    request,
  }) => {
    // The regression this guards was data loss, not cosmetics: the editor
    // loaded items `ORDER BY parent_id` while the save reconstructs each
    // parent from DOM adjacency, so merely *looking* at a menu and
    // clicking Save rewrote its nesting — flattening a child to a root on
    // Postgres, re-parenting it to the wrong node on sqlite.
    const built = await saveTree(request, f.menu.scratch, [
      { id: null, local_id: 0, parent_local_id: null, label: "RootA", page_id: f.page.home },
      { id: null, local_id: 1, parent_local_id: 0, label: "Child", page_id: f.page.about },
      { id: null, local_id: 2, parent_local_id: null, label: "RootB", page_id: f.page.docs },
    ]);
    expect(built.status()).toBeLessThan(400);
    const before = shape((await getJson(request, "/api/v2/menus/t-scratch/")).items);
    expect(before[0].children, "Child starts under RootA").toHaveLength(1);

    await page.goto(`/cms-admin/navigation/${f.menu.scratch}/edit`);
    await page.locator("#menu-builder-save").click();
    await page.waitForTimeout(700);

    const after = shape((await getJson(request, "/api/v2/menus/t-scratch/")).items);
    expect(after, "a read-only visit plus one click must not rewrite the menu").toEqual(before);
  });

  test("a deep tree survives a no-op save", async ({ page, request }) => {
    // Three levels plus a second root — the shape most likely to expose
    // an ordering assumption.
    const built = await saveTree(request, f.menu.scratch, [
      { id: null, local_id: 0, parent_local_id: null, label: "L1", page_id: f.page.home },
      { id: null, local_id: 1, parent_local_id: 0, label: "L2", page_id: f.page.about },
      { id: null, local_id: 2, parent_local_id: 1, label: "L3", page_id: f.page.docs },
      { id: null, local_id: 3, parent_local_id: null, label: "R2", page_id: f.page.guide },
    ]);
    expect(built.status()).toBeLessThan(400);
    const before = shape((await getJson(request, "/api/v2/menus/t-scratch/")).items);

    await page.goto(`/cms-admin/navigation/${f.menu.scratch}/edit`);
    await page.locator("#menu-builder-save").click();
    await page.waitForTimeout(700);

    const after = shape((await getJson(request, "/api/v2/menus/t-scratch/")).items);
    expect(after).toEqual(before);
    // And the ids are the same rows, so translations survived too.
    expect(after[0].id).toBe(before[0].id);
  });

  test("a menu whose sort_order runs backwards vs depth still clones", async ({ request }) => {
    // t-other nests a grandchild (sort_order 10) under a child
    // (sort_order 50). Ordering non-roots by sort_order alone is not
    // topological, so the grandchild used to be inserted before its
    // parent existed — the clone aborted and left a junk `*-copy` menu
    // behind, because it was not transactional.
    const before = await getJson(request, "/api/v2/menus/");
    const token = await csrf(request, f.menu.other);
    const res = await request.post(`/cms-admin/navigation/${f.menu.other}/clone`, {
      headers: { "x-csrf-token": token },
      form: {},
    });
    expect(res.status(), await res.text()).toBeLessThan(400);

    const after = await getJson(request, "/api/v2/menus/?limit=100");
    const copy = after.items.find((m: any) => m.slug.includes("copy"));
    expect(copy, "the clone exists").toBeTruthy();
    expect(after.meta.total_count).toBe(before.meta.total_count + 1);

    // And it is a full copy, nesting intact — not the half-built menu
    // the old ordering left behind.
    const cloned = await getJson(request, `/api/v2/menus/${copy.slug}/`);
    const source = await getJson(request, "/api/v2/menus/t-other/");
    expect(cloned.meta.total_count).toBe(source.meta.total_count);
    expect(labels(cloned.items)).toEqual(labels(source.items));
  });

  test("re-seeding from pages twice does not wipe the menu", async ({ request }) => {
    // The second click used to delete the items in fetch order on the
    // pool, trip the self-FK partway through, and 500 with the menu
    // emptied and nothing re-inserted.
    const token = await csrf(request, f.menu.scratch);
    const seed = () =>
      request.post(`/cms-admin/navigation/${f.menu.scratch}/seed-from-pages`, {
        headers: { "x-csrf-token": token },
        form: {},
      });

    expect((await seed()).status(), "first seed").toBeLessThan(400);
    const first = await getJson(request, "/api/v2/menus/t-scratch/");
    expect(first.meta.total_count).toBeGreaterThan(0);

    expect((await seed()).status(), "second seed").toBeLessThan(400);
    const second = await getJson(request, "/api/v2/menus/t-scratch/");
    expect(second.meta.total_count).toBe(first.meta.total_count);
  });

  test("seeding from pages leaves error pages out of the navbar", async ({ request }) => {
    // The fixture's ErrorPage is marked `show_in_menus`; every other
    // menu surface excludes error pages, and so should this one.
    const token = await csrf(request, f.menu.scratch);
    const res = await request.post(`/cms-admin/navigation/${f.menu.scratch}/seed-from-pages`, {
      headers: { "x-csrf-token": token },
      form: {},
    });
    expect(res.status()).toBeLessThan(400);
    const seeded = await getJson(request, "/api/v2/menus/t-scratch/");
    expect(labels(seeded.items)).not.toContain("T Error");
  });

  test("a menu with a translated item can be deleted outright", async ({ request }) => {
    // `navigation_delete_submit` ignored translations and deleted items
    // in fetch order, so a nested or translated menu 500'd partway and
    // left the menu row behind with some of its items gone.
    const token = await csrf(request, f.menu.scratch);
    const res = await request.post(`/cms-admin/navigation/${f.menu.scratch}/delete`, {
      headers: { "x-csrf-token": token },
      form: {},
    });
    expect(res.status(), await res.text()).toBeLessThan(400);
    expect((await request.get("/api/v2/menus/t-scratch/")).status()).toBe(404);
  });
});
