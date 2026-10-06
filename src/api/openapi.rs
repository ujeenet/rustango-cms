//! `GET /api/v2/openapi.json` — the API describing itself.
//!
//! There was no schema of any kind, so a client had to be hand-written
//! from prose and re-checked by hand whenever anything moved. Response
//! shapes are built with inline `serde_json::json!` literals, so there
//! is no Rust struct to derive a schema from — this describes them
//! explicitly instead.
//!
//! The honest limit: **the response schemas are hand-maintained and can
//! drift from the handlers.** What cannot drift is the *route list* —
//! it comes from [`crate::api::ROUTE_PATHS`], the same constant the
//! router is built from, and a test asserts every path is documented. So
//! a new endpoint can never be silently absent, and a removed one can
//! never linger.
//!
//! Generate a typed client from it:
//!
//! ```sh
//! npx openapi-typescript https://example.com/api/v2/openapi.json -o api.d.ts
//! ```

use axum::response::{IntoResponse, Response};
use axum::Json;

/// Where the document is served.
pub const PATH: &str = "/api/v2/openapi.json";

/// `GET /api/v2/openapi.json`
pub async fn serve() -> Response {
    Json(document()).into_response()
}

/// One endpoint's human- and machine-readable description.
struct Doc {
    path: &'static str,
    summary: &'static str,
    /// `(name, description)` for each query parameter.
    params: &'static [(&'static str, &'static str)],
    /// Component schema name for a 200 body.
    ok: &'static str,
    /// Extra status codes worth calling out, beyond the shared errors.
    extra: &'static [(&'static str, &'static str)],
}

/// Shared across every list endpoint, so they are not re-listed 6 times.
const LIST_PARAMS: &[(&str, &str)] = &[
    ("limit", "Page size, 1–100 (default 20)."),
    ("offset", "Rows to skip."),
    (
        "fields",
        "Comma-separated allowlist; `id` and `meta` are always kept. An unknown name is a 400.",
    ),
    (
        "order",
        "Comma-separated ordering; prefix `-` to reverse, or `random`. An unknown field is a 400.",
    ),
    ("search", "Substring/full-text match on this collection."),
    (
        "updated_since",
        "RFC 3339 instant; only rows touched at or after it. A malformed value is a 400.",
    ),
];

const DOCS: &[Doc] = &[
    Doc {
        path: "/api/v2/pages/",
        summary: "List pages (published and archived), filtered to what the caller may see.",
        params: LIST_PARAMS,
        ok: "PageList",
        extra: &[],
    },
    Doc {
        path: "/api/v2/pages/find/",
        summary: "Resolve a public URL path to a page, as a redirect to its detail URL.",
        params: &[("html_path", "The public URL path, with or without a trailing slash.")],
        ok: "Empty",
        extra: &[
            ("302", "Found — `Location` is the page's detail URL."),
            ("404", "No public page at that path, or the caller may not see it."),
        ],
    },
    Doc {
        path: "/api/v2/pages/tree/",
        summary: "The site's page hierarchy, nested and depth-limited.",
        params: &[
            ("root", "Return the subtree under this page id; the page itself is excluded."),
            ("depth", "Levels to walk, 1–10 (default 3)."),
            ("locale", "Localize titles."),
        ],
        ok: "PageTree",
        extra: &[],
    },
    Doc {
        path: "/api/v2/pages/{id}/",
        summary: "One page, with its extension data, builder body and direct children.",
        params: &[
            ("locale", "Content locale; echoed back as `meta.locale`."),
            ("fields", "Comma-separated allowlist."),
            ("preview_token", "Signed token from the admin, to read a draft."),
        ],
        ok: "PageDetail",
        extra: &[
            ("401", "Authentication required (a gated page, anonymous caller)."),
            ("403", "Authenticated but not permitted."),
        ],
    },
    Doc {
        path: "/api/v2/images/",
        summary: "List images the caller may see, excluding gated collections.",
        params: LIST_PARAMS,
        ok: "MediaList",
        extra: &[],
    },
    Doc {
        path: "/api/v2/images/{id}/",
        summary: "One image, with rendition URLs.",
        params: &[("fields", "Comma-separated allowlist.")],
        ok: "Image",
        extra: &[],
    },
    Doc {
        path: "/api/v2/documents/",
        summary: "List non-image media the caller may see.",
        params: LIST_PARAMS,
        ok: "MediaList",
        extra: &[],
    },
    Doc {
        path: "/api/v2/documents/{id}/",
        summary: "One document.",
        params: &[("fields", "Comma-separated allowlist.")],
        ok: "Document",
        extra: &[],
    },
    Doc {
        path: "/api/v2/snippets/",
        summary: "List snippets (the admin Library).",
        params: LIST_PARAMS,
        ok: "SnippetList",
        extra: &[],
    },
    Doc {
        path: "/api/v2/snippets/{id}/",
        summary: "One snippet, including its `data` bag.",
        params: &[("fields", "Comma-separated allowlist.")],
        ok: "Snippet",
        extra: &[],
    },
    Doc {
        path: "/api/v2/menus/",
        summary: "List configured menus (slug and name only).",
        params: &[
            ("limit", "Page size, 1–100 (default 20)."),
            ("offset", "Rows to skip."),
        ],
        ok: "MenuList",
        extra: &[],
    },
    Doc {
        path: "/api/v2/menus/{slug}/",
        summary: "One menu, resolved into a nested tree with active marking.",
        params: &[
            ("locale", "Localized labels; echoed as `meta.locale`."),
            ("current", "Page id being viewed — marks the active item and its trail."),
        ],
        ok: "MenuDetail",
        extra: &[],
    },
    Doc {
        path: "/api/v2/locales/",
        summary: "Content locales this tenant serves. Inactive ones are omitted.",
        params: &[],
        ok: "LocaleList",
        extra: &[],
    },
    Doc {
        path: "/api/v2/changes/",
        summary: "Per-collection count and last-changed time, for cheap polling.",
        params: &[],
        ok: "Changes",
        extra: &[],
    },
    Doc {
        path: "/api/v2/search/",
        summary: "Search pages, images, documents and snippets in one ranked list.",
        params: &[
            ("q", "The needle. Required."),
            ("type", "Comma-separated subset of page,image,document,snippet."),
            ("limit", "Page size, 1–100 (default 20)."),
            ("offset", "Rows to skip."),
        ],
        ok: "SearchResults",
        extra: &[],
    },
];

/// Build the OpenAPI 3.1 document.
#[must_use]
pub fn document() -> serde_json::Value {
    let mut paths = serde_json::Map::new();
    for d in DOCS {
        let params: Vec<serde_json::Value> = d
            .params
            .iter()
            .map(|(name, desc)| {
                serde_json::json!({
                    "name": name,
                    "in": "query",
                    "required": false,
                    "description": desc,
                    "schema": { "type": "string" },
                })
            })
            .chain(path_params(d.path))
            .collect();

        let mut responses = serde_json::Map::new();
        responses.insert(
            "200".to_owned(),
            serde_json::json!({
                "description": "Success",
                "content": { "application/json": {
                    "schema": { "$ref": format!("#/components/schemas/{}", d.ok) }
                }},
            }),
        );
        responses.insert("304".to_owned(), serde_json::json!({
            "description": "Not Modified — the `If-None-Match` validator still matches.",
        }));
        for (code, desc) in d.extra {
            responses.insert(
                (*code).to_owned(),
                serde_json::json!({
                    "description": desc,
                    "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/Error" }
                    }},
                }),
            );
        }
        for (code, desc) in [
            ("400", "Malformed request — see `error.code`."),
            ("404", "No such resource, or the caller may not see it."),
            ("405", "Write verb on a read-only API. `Allow` names what this path serves."),
            ("500", "Server error."),
        ] {
            responses.entry(code.to_owned()).or_insert_with(|| {
                serde_json::json!({
                    "description": desc,
                    "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/Error" }
                    }},
                })
            });
        }

        paths.insert(
            d.path.to_owned(),
            serde_json::json!({ "get": {
                "summary": d.summary,
                "parameters": params,
                "responses": responses,
            }}),
        );
    }

    paths.insert(
        crate::api::auth::LOGIN_PATH.to_owned(),
        serde_json::json!({ "post": {
            "summary": "Exchange member credentials for a bearer token.",
            "description":
                "The member session cookie is `SameSite=Lax` and is not sent on \
                 cross-site fetches, so a SPA on another origin cannot use it. \
                 This returns the same signed session value as a token to send \
                 as `Authorization: Bearer`.",
            "requestBody": { "required": true, "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/LoginRequest" }
                },
                "application/x-www-form-urlencoded": {
                    "schema": { "$ref": "#/components/schemas/LoginRequest" }
                },
            }},
            "responses": {
                "200": { "description": "Signed in", "content": { "application/json": {
                    "schema": { "$ref": "#/components/schemas/LoginResponse" }
                }}},
                "400": { "description": "Malformed body", "content": { "application/json": {
                    "schema": { "$ref": "#/components/schemas/Error" }
                }}},
                "401": { "description": "Invalid credentials", "content": { "application/json": {
                    "schema": { "$ref": "#/components/schemas/Error" }
                }}},
            },
        }}),
    );

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "rustango-cms content API",
            "version": "2",
            "description":
                "Read-only JSON over the CMS's public content. Every endpoint is \
                 viewer-aware: gated pages and assets in closed collections are \
                 absent for callers who may not see them. Responses carry a weak \
                 ETag and `Cache-Control: private, no-cache`; send the tag back as \
                 `If-None-Match` for a 304.",
        },
        "paths": paths,
        "components": {
            "schemas": schemas(),
            "securitySchemes": { "bearerAuth": { "type": "http", "scheme": "bearer" } },
        },
    })
}

/// `{id}` / `{slug}` captures declared as path parameters.
fn path_params(path: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    path.split('/')
        .filter(|seg| seg.starts_with('{') && seg.ends_with('}'))
        .map(|seg| {
            let name = seg.trim_matches(|c| c == '{' || c == '}');
            serde_json::json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": if name == "slug" { "string" } else { "integer" } },
            })
        })
}

fn obj(props: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": props })
}

fn arr(item_ref: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "items": { "$ref": format!("#/components/schemas/{item_ref}") },
    })
}

fn schemas() -> serde_json::Value {
    let str_t = serde_json::json!({ "type": "string" });
    let int_t = serde_json::json!({ "type": "integer" });
    let bool_t = serde_json::json!({ "type": "boolean" });
    let nullable_str = serde_json::json!({ "type": ["string", "null"] });

    serde_json::json!({
        "Error": obj(serde_json::json!({
            "error": obj(serde_json::json!({
                "code": {
                    "type": "string",
                    "enum": ["not_found", "bad_request", "unauthenticated",
                             "forbidden", "password_required", "method_not_allowed",
                             "internal_error"],
                },
                "message": str_t,
            })),
        })),
        "ListMeta": obj(serde_json::json!({
            "total_count": int_t, "limit": int_t, "offset": int_t,
            "has_more": bool_t, "next_offset": int_t, "previous_offset": int_t,
        })),
        "PageSummary": obj(serde_json::json!({
            "id": int_t,
            "title": str_t,
            "url": str_t,
            "seo_title": str_t,
            "seo_description": str_t,
            "meta": obj(serde_json::json!({
                "type": str_t, "detail_url": str_t, "html_url": str_t,
                "slug": str_t, "depth": int_t, "parent_id": int_t,
                "alias_of": int_t, "first_published_at": nullable_str,
                "updated_at": nullable_str,
            })),
        })),
        "PageList": obj(serde_json::json!({
            "meta": { "$ref": "#/components/schemas/ListMeta" },
            "items": arr("PageSummary"),
        })),
        "ChildSummary": obj(serde_json::json!({
            "id": int_t, "title": str_t, "slug": str_t, "url": str_t,
            "has_children": bool_t, "detail_url": str_t,
        })),
        "PageDetail": obj(serde_json::json!({
            "id": int_t, "title": str_t, "url": str_t,
            "meta": obj(serde_json::json!({ "type": str_t, "locale": nullable_str })),
            "extension": { "type": "object" },
            "builder": { "type": "object" },
            "children": arr("ChildSummary"),
        })),
        "TreeNode": obj(serde_json::json!({
            "id": int_t, "title": str_t, "type": str_t, "slug": str_t,
            "url": str_t, "depth": int_t, "has_children": bool_t,
            "detail_url": str_t, "alias_of": int_t,
            "children": arr("TreeNode"),
        })),
        "PageTree": obj(serde_json::json!({
            "meta": obj(serde_json::json!({
                "depth": int_t, "root": int_t, "locale": nullable_str,
                "total_count": int_t, "truncated": bool_t, "dropped_count": int_t,
            })),
            "items": arr("TreeNode"),
        })),
        "Image": obj(serde_json::json!({
            "id": int_t, "title": str_t, "filename": str_t, "alt_text": str_t,
            "mime": str_t, "size": int_t, "width": int_t, "height": int_t,
            "collection_id": int_t,
            "renditions": obj(serde_json::json!({
                "thumbnail": nullable_str, "medium": nullable_str, "large": nullable_str,
            })),
            "meta": obj(serde_json::json!({
                "type": str_t, "detail_url": str_t, "download_url_template": str_t,
            })),
        })),
        "Document": obj(serde_json::json!({
            "id": int_t, "title": str_t, "filename": str_t, "mime": str_t,
            "kind": str_t, "size": int_t, "collection_id": int_t,
            "meta": obj(serde_json::json!({ "type": str_t, "detail_url": str_t })),
        })),
        "MediaList": obj(serde_json::json!({
            "meta": { "$ref": "#/components/schemas/ListMeta" },
            "items": { "type": "array", "items": { "type": "object" } },
        })),
        "Snippet": obj(serde_json::json!({
            "id": int_t, "type_name": str_t, "slug": str_t, "title": str_t,
            "folder_path": str_t, "body_markdown": str_t,
            "data": { "type": "object" },
            "meta": obj(serde_json::json!({
                "type": str_t, "type_name": str_t, "detail_url": str_t,
                "updated_at": nullable_str,
            })),
        })),
        "SnippetList": obj(serde_json::json!({
            "meta": { "$ref": "#/components/schemas/ListMeta" },
            "items": arr("Snippet"),
        })),
        "MenuSummary": obj(serde_json::json!({
            "slug": str_t, "name": str_t, "detail_url": str_t,
        })),
        "MenuList": obj(serde_json::json!({
            "meta": { "$ref": "#/components/schemas/ListMeta" },
            "items": arr("MenuSummary"),
        })),
        "MenuItem": obj(serde_json::json!({
            "id": int_t,
            "label": str_t,
            "url": nullable_str,
            "is_page": bool_t,
            "page_id": int_t,
            "open_in_new_tab": bool_t,
            "is_active": bool_t,
            "in_active_trail": bool_t,
            "children": arr("MenuItem"),
        })),
        "MenuDetail": obj(serde_json::json!({
            "meta": obj(serde_json::json!({
                "slug": str_t, "name": str_t, "locale": nullable_str,
                "current": int_t, "total_count": int_t,
            })),
            "items": arr("MenuItem"),
        })),
        "Locale": obj(serde_json::json!({
            "code": str_t, "name": str_t, "is_default": bool_t,
        })),
        "LocaleList": obj(serde_json::json!({
            "meta": obj(serde_json::json!({ "total_count": int_t, "default": nullable_str })),
            "items": arr("Locale"),
        })),
        "SearchHit": obj(serde_json::json!({
            "type": { "type": "string", "enum": ["page", "image", "document", "snippet"] },
            "id": int_t, "title": str_t, "url": nullable_str,
            "detail_url": str_t, "score": { "type": "number" },
        })),
        "SearchResults": obj(serde_json::json!({
            "meta": obj(serde_json::json!({
                "q": str_t, "types": { "type": "array", "items": str_t },
                "total_count": int_t, "limit": int_t, "offset": int_t, "has_more": bool_t,
            })),
            "items": arr("SearchHit"),
        })),
        "ChangeEntry": obj(serde_json::json!({
            "count": int_t,
            "latest": nullable_str,
        })),
        "Changes": obj(serde_json::json!({
            "meta": obj(serde_json::json!({ "checked_at": str_t, "usage": str_t })),
            "collections": obj(serde_json::json!({
                "pages": { "$ref": "#/components/schemas/ChangeEntry" },
                "images": { "$ref": "#/components/schemas/ChangeEntry" },
                "documents": { "$ref": "#/components/schemas/ChangeEntry" },
                "snippets": { "$ref": "#/components/schemas/ChangeEntry" },
                "menus": { "$ref": "#/components/schemas/ChangeEntry" },
            })),
        })),
        "LoginRequest": obj(serde_json::json!({
            "identifier": str_t, "password": str_t,
        })),
        "LoginResponse": obj(serde_json::json!({
            "token": str_t, "token_type": str_t, "expires_in": int_t,
            "member": obj(serde_json::json!({
                "id": int_t, "username": str_t, "email": nullable_str,
            })),
        })),
        "Empty": { "type": "object" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant that keeps this honest: the schema describes exactly
    /// the routes the router serves. Add an endpoint without documenting
    /// it — or document one that no longer exists — and this fails.
    #[test]
    fn every_route_is_documented_and_nothing_extra() {
        let documented: std::collections::BTreeSet<&str> =
            DOCS.iter().map(|d| d.path).collect();
        let served: std::collections::BTreeSet<&str> =
            crate::api::ROUTE_PATHS.iter().copied().collect();
        assert_eq!(
            documented, served,
            "the OpenAPI path list and the router's ROUTE_PATHS disagree",
        );
    }

    #[test]
    fn every_endpoint_has_a_summary() {
        for d in DOCS {
            assert!(!d.summary.is_empty(), "{} has no summary", d.path);
            assert!(
                d.summary.ends_with('.'),
                "{} summary should read as a sentence",
                d.path,
            );
        }
    }

    #[test]
    fn every_referenced_schema_exists() {
        // A dangling `$ref` produces a client that fails to generate, and
        // the failure names the ref rather than the endpoint.
        let doc = document();
        let defined: std::collections::BTreeSet<String> = doc["components"]["schemas"]
            .as_object()
            .expect("schemas")
            .keys()
            .cloned()
            .collect();
        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);
        for r in refs {
            let name = r.rsplit('/').next().unwrap_or_default().to_owned();
            assert!(defined.contains(&name), "dangling $ref: {r}");
        }
    }

    fn collect_refs(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    if k == "$ref" {
                        if let Some(s) = val.as_str() {
                            out.push(s.to_owned());
                        }
                    }
                    collect_refs(val, out);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| collect_refs(i, out)),
            _ => {}
        }
    }

    #[test]
    fn path_captures_become_path_parameters() {
        let params: Vec<serde_json::Value> = path_params("/api/v2/menus/{slug}/").collect();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0]["name"], "slug");
        assert_eq!(params[0]["in"], "path");
        assert_eq!(params[0]["required"], true);
        assert_eq!(params[0]["schema"]["type"], "string");

        let params: Vec<serde_json::Value> = path_params("/api/v2/pages/{id}/").collect();
        assert_eq!(params[0]["schema"]["type"], "integer");

        assert_eq!(path_params("/api/v2/pages/").count(), 0);
    }

    #[test]
    fn the_document_is_a_usable_openapi_31_shell() {
        let doc = document();
        assert_eq!(doc["openapi"], "3.1.0");
        assert!(doc["info"]["title"].is_string());
        // Every path is a GET with responses.
        for path in crate::api::ROUTE_PATHS {
            let op = &doc["paths"][path]["get"];
            assert!(op.is_object(), "{path} has no GET operation");
            assert!(op["responses"]["200"].is_object(), "{path} has no 200");
            assert!(op["responses"]["404"].is_object(), "{path} has no 404");
        }
    }
}
