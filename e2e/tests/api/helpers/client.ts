/**
 * A thin API client with the manners a real SPA needs.
 *
 * The one rule worth encoding: **check `content-type` before parsing.**
 * Every r-cms error body is `text/plain` (G7), so a bare
 * `await res.json()` on a failure throws `SyntaxError: Unexpected token`
 * and the spec reports a JSON parse error instead of the 403 that
 * actually happened. `json()` here refuses to guess.
 */
import { APIRequestContext, APIResponse, expect, request } from "@playwright/test";

export type Json = any;

/** Parse a JSON body, or fail with what the server actually sent. */
export async function json(res: APIResponse): Promise<Json> {
  const ct = res.headers()["content-type"] ?? "";
  if (!ct.includes("application/json")) {
    const body = (await res.text()).slice(0, 300);
    throw new Error(
      `expected JSON from ${res.url()} but got ${res.status()} ${ct || "<no content-type>"}: ${body}`,
    );
  }
  return res.json();
}

/** `GET` and assert 200 + JSON in one step — the happy path most specs want. */
export async function getJson(api: APIRequestContext, path: string): Promise<Json> {
  const res = await api.get(path);
  expect(res.status(), `GET ${path}`).toBe(200);
  return json(res);
}

/**
 * A context that does **not** follow redirects, so a 302/303 is
 * observable. The default context follows them, which is precisely how
 * a `fetch()`-based SPA ends up with an HTML login page under status 200
 * (G6) — several specs need to see the redirect itself.
 */
export async function rawContext(baseURL: string): Promise<APIRequestContext> {
  return request.newContext({ baseURL, maxRedirects: 0 });
}

/** An anonymous context — no cookies at all. The leak matrix depends on this. */
export async function anonContext(baseURL: string): Promise<APIRequestContext> {
  return request.newContext({ baseURL, storageState: undefined });
}

// ---------------------------------------------------------------------
// Shape helpers. The API returns three different shapes for one concept
// (B47), so specs need to reach into each of them without repeating the
// key-name trivia.
// ---------------------------------------------------------------------

/** Every node in a nested `items` tree, depth-first. */
export function flatten(items: Json): Json[] {
  const out: Json[] = [];
  const walk = (arr: Json) => {
    if (!Array.isArray(arr)) return;
    for (const n of arr) {
      out.push(n);
      walk(n.children);
    }
  };
  walk(items);
  return out;
}

/** Depth-first `title`s (page tree / child summaries). */
export const titles = (items: Json): string[] =>
  flatten(items).map((n) => n.title).filter((t) => typeof t === "string");

/** Depth-first `label`s (menus). */
export const labels = (items: Json): string[] =>
  flatten(items).map((n) => n.label).filter((l) => typeof l === "string");

/** Find a node anywhere in a nested tree by one of its string fields. */
export const findNode = (items: Json, key: string, want: string): Json | undefined =>
  flatten(items).find((n) => n[key] === want);

/**
 * The page's URL, whichever shape it arrived in — `meta.html_url` on a
 * list item, `url` on a tree node or child summary (B47).
 */
export const urlOf = (node: Json): string | undefined => node?.url ?? node?.meta?.html_url;

/** The page's id, list-item or node shape alike. */
export const idOf = (node: Json): number | undefined => node?.id;

/** Ids in a list response, in order — the pagination specs compare these. */
export const listIds = (body: Json): number[] => (body.items ?? []).map((i: Json) => i.id);
