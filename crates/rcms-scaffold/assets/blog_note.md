
## Typed extension (blog template)

`ArticlePage` carries a typed extension table `cms_article_page` (Markdown body + optional hero image), edited inline in the page editor.

That table is **your app's schema**, not the CMS's, so no shipped migration creates it — generate one before the first run (never hand-author migration JSON):

```sh
# SQLite
cargo run --no-default-features --features sqlite -- makemigrations   # emits cms_article_page
cargo run --no-default-features --features sqlite -- migrate-tenants

# PostgreSQL
cargo run -- makemigrations
cargo run -- migrate-tenants
```

Re-run both any time you change `ArticleBody` in `src/models.rs`.

The article template renders it via `{{ extension.body_markdown | markdown | safe }}` + `rcms_image_url(media_id=extension.hero_media_id, …)`. The `| safe` is required — the markdown filter sanitizes its own output, and without it Tera escapes the tags and readers see them as text.
