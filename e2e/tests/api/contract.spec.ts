/**
 * Cross-endpoint agreement.
 *
 * One page, fetched every way the API offers it. A SPA holds a single
 * `Page` model and hydrates it from whichever endpoint is cheapest, so
 * anywhere these disagree is a bug the client has to carry forever.
 */
import { expect, test } from "@playwright/test";
import { findNode, getJson, urlOf } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();

/** T Docs, as each surface describes it. */
async function views(request: any) {
  // Searched rather than paged: T Home parents 200+ bulk pages, so
  // `?child_of=` with the 100-row cap would not reach T Docs.
  const list = await getJson(request, "/api/v2/pages/?search=t-docs&limit=100");
  const tree = await getJson(request, `/api/v2/pages/tree/?root=${f.page.home}&depth=1`);
  const parent = await getJson(request, `/api/v2/pages/${f.page.home}/`);
  return {
    listItem: list.items.find((i: any) => i.id === f.page.docs),
    treeNode: findNode(tree.items, "title", "T Docs"),
    childSummary: parent.children.find((c: any) => c.id === f.page.docs),
    detail: await getJson(request, `/api/v2/pages/${f.page.docs}/`),
  };
}

test.describe("cross-endpoint contract", () => {
  test("every surface agrees on the page's id and title", async ({ request }) => {
    const v = await views(request);
    for (const [name, node] of Object.entries(v)) {
      expect(node, `${name} did not return T Docs at all`).toBeTruthy();
      expect(node.id, `${name}.id`).toBe(f.page.docs);
      expect(node.title, `${name}.title`).toBe("T Docs");
    }
  });

  test("every surface agrees on the page's URL", async ({ request }) => {
    const v = await views(request);
    // `urlOf` exists precisely because the key name differs: `url` on a
    // tree node and child summary, `meta.html_url` on a list item.
    for (const [name, node] of Object.entries(v)) {
      expect(urlOf(node), `${name} URL`).toBe("/t-home/t-docs");
    }
  });

  test("a detail_url handed out by the tree actually resolves", async ({ request }) => {
    // Regression for the archived fix: `tree` lists archived pages and
    // emits a detail_url for each; `detail` used to 404 them.
    const tree = await getJson(request, "/api/v2/pages/tree/?depth=10");
    const archived = findNode(tree.items, "title", "T Archived");
    expect(archived, "the tree serves archived pages").toBeTruthy();
    const res = await request.get(archived.detail_url);
    expect(res.status(), `${archived.detail_url} came from the tree and must resolve`).toBe(200);
  });

  test("every child summary's detail_url resolves", async ({ request }) => {
    const parent = await getJson(request, `/api/v2/pages/${f.page.home}/`);
    for (const child of parent.children.slice(0, 12)) {
      const res = await request.get(child.detail_url);
      expect(res.status(), `${child.title} → ${child.detail_url}`).toBe(200);
    }
  });

  test("a menu item's page_id resolves through the detail endpoint", async ({ request }) => {
    const menu = await getJson(request, "/api/v2/menus/t-main/");
    const docs = findNode(menu.items, "label", "T Docs link");
    const detail = await getJson(request, `/api/v2/pages/${docs.page_id}/`);
    expect(detail.meta.html_url).toBe(docs.url);
  });

  // -------------------------------------------------------------------

  test("one url key and one type key work on every shape", async ({ request }) => {
    // A client used to need `meta.html_url` on a list item and `url` on a
    // tree node or child summary, and there was no type on a tree node at
    // all — so no single deserializer covered them.
    const v = await views(request);
    for (const [name, node] of Object.entries(v)) {
      expect(node.url, `${name}.url`).toBe("/t-home/t-docs");
    }
    expect(v.treeNode.type, "tree nodes report their type").toBe("ArticlePage");
    expect(v.listItem.meta.type).toBe("ArticlePage");
  });

  test("list and tree describe the same site", async ({ request }) => {
    const list = await getJson(request, "/api/v2/pages/?limit=100&order=title");
    const tree = await getJson(request, "/api/v2/pages/tree/?depth=10");
    const inList = list.items.some((i: any) => i.title === "T Archived");
    const inTree = findNode(tree.items, "title", "T Archived") !== undefined;
    expect(inList).toBe(inTree);
    expect(inList, "archived is public and both surfaces serve it").toBe(true);
  });

  test("tree and detail agree about an alias's title", async ({ request }) => {
    const tree = await getJson(request, "/api/v2/pages/tree/?depth=10");
    const aliasNode = tree.items
      .flatMap((n: any) => n.children ?? [])
      .find((n: any) => n.id === f.page.alias);
    expect(aliasNode, "the alias appears as its own node").toBeTruthy();
    const detail = await getJson(request, `/api/v2/pages/${f.page.alias}/`);
    expect(detail.title).toBe(aliasNode.title);
    expect(aliasNode.alias_of).toBe(f.page.docs);
  });
});
