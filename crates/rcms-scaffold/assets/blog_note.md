
## Typed extension (blog template)

`ArticlePage` carries a typed extension table `cms_article_page` (Markdown body + optional hero image), edited inline in the page editor.

That table is **your app's schema**, not the CMS's, so no shipped migration creates it — generate one before the first run (never hand-author migration JSON):

```sh
cargo run -- makemigrations   # emits cms_article_page
cargo run -- quickstart        # creates the site with it
```

Each time you change `ArticleBody` in `src/models.rs` afterwards, run
`cargo run -- makemigrations` and `cargo run -- migrate-tenants`.

The article template renders it via `{{ extension.body_markdown | markdown | safe }}` + `rcms_image_url(media_id=extension.hero_media_id, …)`. The `| safe` is required — the markdown filter sanitizes its own output, and without it Tera escapes the tags and readers see them as text.
