#!/usr/bin/env python3
"""Full-scale MCP exercise against the demo tenant — every tool, at scale,
with result verification. Uses the raw prefix.secret keys directly as
Bearer tokens (the copy-paste path).

Mint two keys in the demo admin (Account → MCP keys: one with every skill,
one read-only) and pass them in the environment:

    MCP_FULL_KEY=... MCP_READ_KEY=... python3 e2e/mcp/fullscale_smoke.py
"""
import json, os, urllib.request, sys

MCP = os.environ.get("MCP_URL", "http://demo.localhost:8210/cms-admin/mcp")
FULL = os.environ["MCP_FULL_KEY"].strip()
READ = os.environ["MCP_READ_KEY"].strip()

_id = [0]
def rpc(token, method, params=None):
    _id[0] += 1
    body = json.dumps({"jsonrpc": "2.0", "id": _id[0], "method": method,
                       "params": params or {}}).encode()
    req = urllib.request.Request(MCP, data=body, headers={
        "content-type": "application/json", "authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(req) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        return {"http_error": e.code, "body": e.read().decode()[:200]}

def call(token, name, args):
    r = rpc(token, "tools/call", {"name": name, "arguments": args})
    if "error" in r:
        return {"_rpc_error": r["error"]}
    res = r.get("result", {})
    if res.get("isError"):
        return {"_tool_error": res.get("content")}
    return res.get("structuredContent", res)

PASS, FAIL = [], []
def check(label, cond, detail=""):
    (PASS if cond else FAIL).append(label)
    print(f"  {'✓' if cond else '✗'} {label}" + (f"  {detail}" if detail and not cond else ""))

print("== 1. handshake + discovery ==")
init = rpc(FULL, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "fullscale", "version": "1"}})
check("initialize", init.get("result", {}).get("serverInfo", {}).get("name") == "rustango")
tools_full = sorted(t["name"] for t in rpc(FULL, "tools/list")["result"]["tools"])
check("full key lists 14 tools", len(tools_full) == 14, str(tools_full))
tools_read = sorted(t["name"] for t in rpc(READ, "tools/list")["result"]["tools"])
check("read key lists only 8 read tools", len(tools_read) == 8, str(tools_read))
check("read key excludes create_page", "create_page" not in tools_read)

print("== 2. discovery tools ==")
pt = call(FULL, "list_page_types", {})
type_names = [t["type_name"] for t in pt["types"]]
check("list_page_types returns types", len(type_names) >= 3, str(type_names))
check("list_page_types returns blocks", len(pt["blocks"]) >= 1)
has_stream = any(t["type_name"] == "SectionedPage" for t in pt["types"])
check("SectionedPage present", has_stream)
locales = call(FULL, "list_locales", {})
codes = [l["code"] for l in locales["locales"]]
check("locales include fr/es/de", all(c in codes for c in ["fr", "es", "de"]), str(codes))

print("== 3. read key is fail-closed on writes ==")
denied = call(READ, "create_page", {"page_type": "SectionedPage", "title": "nope"})
check("read key create_page denied", "_rpc_error" in denied and denied["_rpc_error"]["code"] == -32003,
      json.dumps(denied))

print("== 4. create pages at scale (nested stream bodies) ==")
created = []
for i in range(1, 13):
    r = call(FULL, "create_page", {
        "page_type": "SectionedPage",
        "title": f"Full-Scale Page {i:02d}",
        "status": "draft",
        "body": [
            {"type": "heading", "value": {"text": f"Section {i} heading"}},
            {"type": "paragraph", "value": {"body": f"Body copy for page {i}. " * 3}},
        ],
    })
    pid = r.get("page", {}).get("id")
    if pid:
        created.append(pid)
check("created 12 pages", len(created) == 12, f"got {len(created)}")

print("== 5. child placement + validation ==")
# nested child under the first page
child = call(FULL, "create_page", {
    "parent_id": created[0], "page_type": "SectionedPage",
    "title": "A Child Page", "body": [{"type": "heading", "value": {"text": "child"}}]})
check("child page created under parent", child.get("page", {}).get("id") is not None, json.dumps(child)[:160])
# invalid block type is rejected
badblock = call(FULL, "create_page", {"page_type": "SectionedPage", "title": "Bad",
                                      "body": [{"type": "not_a_real_block", "value": {}}]})
check("invalid block type rejected", "_rpc_error" in badblock and badblock["_rpc_error"]["code"] == -32602)
# unknown page type is rejected
badtype = call(FULL, "create_page", {"page_type": "Nonexistent", "title": "Bad"})
check("unknown page_type rejected", "_rpc_error" in badtype)

print("== 6. search + read-back ==")
found = call(FULL, "search_pages", {"query": "Full-Scale", "limit": 50})
check("search finds all 12", len([p for p in found["pages"] if "Full-Scale" in p["title"]]) == 12,
      str(len(found["pages"])))
gp = call(FULL, "get_page", {"page_id": created[0]})
body = json.loads(gp.get("fields", {}).get("body", "[]"))
check("get_page returns 2 blocks with server-minted UUIDs",
      len(body) == 2 and all(b.get("id") for b in body), json.dumps(body)[:160])

print("== 7. update + publish ==")
upd = call(FULL, "update_page", {"page_id": created[1], "title": "Full-Scale Page 02 (edited)",
                                 "seo_title": "SEO for page 2"})
check("update_page title", upd.get("page", {}).get("title") == "Full-Scale Page 02 (edited)")
pubbed = 0
for pid in created[:6]:
    r = call(FULL, "publish_page", {"page_id": pid})
    if r.get("page", {}).get("status") == "published":
        pubbed += 1
check("published 6 pages", pubbed == 6, f"got {pubbed}")
pub_search = call(FULL, "search_pages", {"status": "published", "limit": 50})
check("search status=published >= 6", len(pub_search["pages"]) >= 6, str(len(pub_search["pages"])))

print("== 8. translations (fr + es) ==")
fields = call(FULL, "list_translatable_fields", {"page_id": created[0]})
paths = [f["field_path"] for f in fields["fields"]]
check("translatable fields include title", "title" in paths, str(paths[:5]))
check("translatable fields include a stream block leaf",
      any(p.startswith("body.") and p.count(".") >= 2 for p in paths), str(paths))
tr_fr = call(FULL, "upsert_translations", {"page_id": created[0], "locale": "fr",
             "updates": [{"field_path": "title", "value": "Page à grande échelle 01"}]})
check("fr translation applied", tr_fr.get("applied") == 1, json.dumps(tr_fr))
tr_es = call(FULL, "upsert_translations", {"page_id": created[0], "locale": "es",
             "updates": [{"field_path": "title", "value": "Página a gran escala 01"}]})
check("es translation applied", tr_es.get("applied") == 1, json.dumps(tr_es))
# default locale rejected
tr_def = call(FULL, "upsert_translations", {"page_id": created[0], "locale": "en",
             "updates": [{"field_path": "title", "value": "x"}]})
check("default-locale translation rejected", "_rpc_error" in tr_def)
# delete via empty value
tr_del = call(FULL, "upsert_translations", {"page_id": created[0], "locale": "es",
             "updates": [{"field_path": "title", "value": ""}]})
check("empty value deletes override", tr_del.get("applied") == 1)

print("== 8b. nested stream blocks (typed_table → rows) ==")
nested = call(FULL, "create_page", {
    "page_type": "SectionedPage", "title": "Nested Blocks", "status": "published",
    "body": [
        {"type": "heading", "value": {"text": "Pricing"}},
        {"type": "typed_table", "value": {"caption": "Plans", "rows": [
            {"type": "typed_table_row", "value": {"label": "Basic", "value": 10, "notes": "per month"}},
            {"type": "typed_table_row", "value": {"label": "Pro", "value": 30, "notes": "per month"}},
        ]}},
    ]})
npid = nested.get("page", {}).get("id")
check("nested create ok", npid is not None, json.dumps(nested)[:160])
ngp = call(FULL, "get_page", {"page_id": npid})
nbody = json.loads(ngp.get("fields", {}).get("body", "[]"))
tt = next((b for b in nbody if b["type"] == "typed_table"), None)
rows = tt["value"].get("rows", []) if tt else []
check("2 nested rows persisted with UUIDs",
      len(rows) == 2 and tt.get("id") and all(r.get("id") for r in rows), json.dumps(rows)[:200])
check("nested row values round-trip", rows and rows[0]["value"]["label"] == "Basic")
nfields = [f["field_path"] for f in call(FULL, "list_translatable_fields", {"page_id": npid})["fields"]]
check("row-level leaves exposed with deep dotted paths",
      any(".rows." in p and p.endswith(".label") for p in nfields), str([p for p in nfields if ".rows." in p][:2]))
deep = next((p for p in nfields if ".rows." in p and p.endswith(".label")), None)
tr_nested = call(FULL, "upsert_translations", {"page_id": npid, "locale": "fr",
               "updates": [{"field_path": deep, "value": "Basique"}]}) if deep else {}
check("nested-leaf translation applies", tr_nested.get("applied") == 1, json.dumps(tr_nested))

print("== 8c. deep-tree index links + published-only children_query ==")
# Section (index) -> Guide (child) -> Install (grandchild); + a draft child.
sec = call(FULL, "create_page", {"page_type": "SectionedPage", "title": "Docs Hub",
           "status": "published", "body": [{"type": "heading", "value": {"text": "Docs"}}]})
sid = sec["page"]["id"]
guide = call(FULL, "create_page", {"parent_id": sid, "page_type": "SectionedPage",
             "title": "Guide", "status": "published", "body": [{"type": "heading", "value": {"text": "Guide"}}]})
gpath = guide["page"]["url_path"]
inst = call(FULL, "create_page", {"parent_id": guide["page"]["id"], "page_type": "SectionedPage",
            "title": "Install", "status": "published", "body": [{"type": "heading", "value": {"text": "Install"}}]})
ipath = inst["page"]["url_path"]
call(FULL, "create_page", {"parent_id": sid, "page_type": "SectionedPage",
     "title": "Hidden Draft", "status": "draft"})
import urllib.request as _u
def get_html(path): return _u.urlopen(MCP.split("/cms-admin")[0] + path).read().decode()
docs_html = get_html(sec["page"]["url_path"])
check("index links published child by full url_path",
      f'href="{gpath}"' in docs_html or gpath.replace("/", "&#x2F;") in docs_html, gpath)
check("published-only children_query hides the draft", "Hidden Draft" not in docs_html)
guide_html = get_html(gpath)
# the grandchild's FULL deep path, not the leaf slug (the fixed bug)
check("child index links grandchild by FULL deep path",
      f'href="{ipath}"' in guide_html or ipath.replace("/", "&#x2F;") in guide_html, ipath)
check("no leaf-slug link (/install)", 'href="/install"' not in guide_html)
inst_html = get_html(ipath)
check("grandchild breadcrumb links ancestor by full path",
      f'href="{gpath}"' in inst_html or gpath.replace("/", "&#x2F;") in inst_html, gpath)

print("== 9. media upload (dedupe) ==")
PNG = ("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
m1 = call(FULL, "upload_media", {"filename": "pixel.png", "content_base64": PNG,
                                 "title": "Full-scale pixel", "alt_text": "one pixel"})
mid = m1.get("media", {}).get("id")
check("media uploaded", mid is not None and m1["media"]["url"], json.dumps(m1)[:160])
m2 = call(FULL, "upload_media", {"filename": "pixel-copy.png", "content_base64": PNG,
                                 "title": "dupe"})
check("dedupe returns same id", m2.get("media", {}).get("id") == mid, json.dumps(m2)[:120])
media_list = call(FULL, "list_media", {})
check("list_media shows the upload", any(it.get("id") == mid for it in media_list.get("items", [])),
      str(media_list.get("items", []))[:120])

print("== 10. snippets ==")
sn = call(FULL, "upsert_snippet", {"type_name": "form", "title": "Contact (MCP)",
          "data": {"fields": []}})
sid = sn.get("snippet", {}).get("id")
check("snippet created", sid is not None, json.dumps(sn)[:160])
if sid:
    sn2 = call(FULL, "upsert_snippet", {"type_name": "form", "id": sid, "title": "Contact (MCP, v2)"})
    check("snippet updated", sn2.get("snippet", {}).get("title") == "Contact (MCP, v2)", json.dumps(sn2)[:120])
sl = call(FULL, "list_snippets", {})
check("list_snippets shows it", any(s["id"] == sid for s in sl.get("snippets", [])))

print("== 11. collections ==")
cols = call(FULL, "list_collections", {})
check("list_collections returns tree", "collections" in cols)

print(f"\n==== RESULT: {len(PASS)} passed, {len(FAIL)} failed ====")
if FAIL:
    print("FAILED:", FAIL)
    sys.exit(1)
