# rustango-cms-macros

The procedural macros behind [rustango-cms](https://github.com/ujeenet/rustango-cms):
`#[derive(PageType)]` with its `#[field(...)]` attributes, and the block derive.

You don't add this crate yourself — rustango-cms re-exports the derives:

```rust
use rustango::Model;
use rustango::sql::Auto;
use rustango_cms::{PageType, PageTypeOverrides};

#[derive(Model, PageType, Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "blog_post_page_ext", app = "blog")]
#[page_type(type_name = "BlogPostPage", verbose_name = "Blog post", template = "blog_post.html")]
pub struct BlogPostPage {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(widget = Markdown, label = "Body", help = "GFM.")]
    pub body_markdown: String,
}

impl PageTypeOverrides for BlogPostPage {}
```

See the rustango-cms README for how page types work.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
