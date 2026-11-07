# The helper traits

**Goal:** understand the parts of the Clay & Kiln shop that are made in Rust, and make your own.

**Who this is for:** Rust developers who build a site with Rustango-CMS.

**Time:** about 20 minutes.

This is a developer chapter of [Build a ceramics shop, step by step](shop-overview.md). All code is from `examples/ceramics_shop/models.rs`.

## Code or admin?

Most of the shop is made in the admin: the Product type, the categories, the menus, the settings values. Code is only needed when the admin cannot do it:

| You need… | In the admin | In code |
|---|---|---|
| A page type with simple fields | **Page types → New page type** ([chapter 4](shop-product-type.md)) | |
| A page type with its own table, or its own logic (a special list of children, extra data for the template) | | `#[derive(PageType)]` + `PageTypeOverrides` |
| A kind of reusable content for the Library | | `LibraryTypeHandler` + `register_library_type!` |
| A new vocabulary next to **Categories** | | `TaxonomyHandler` + `register_taxonomy!` |
| A settings group with fixed boxes | | `register_site_setting!` |
| The values of settings, menus, categories | the admin | |
| Your own table, edited in the admin | | `#[derive(CmsAdmin)]` — see [Your own data in the admin](shop-dev-admin-screens.md) |
| An admin screen that does one job (an import, a report) | | `AdminPageHandler` + `register_admin_page!` — same chapter |
| Your own version of a built-in block | | a block with the same name (see "Replace a built-in block") |

## `#[derive(PageType)]` — a page type in code

The shop's **Workshop** type (a pottery class) has a date, a number of seats, a photo and a description:

```rust
#[derive(rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "shop_workshop", app = "shop")]
#[page_type(type_name = "Workshop", verbose_name = "Workshop", template = "workshop.html", icon = "event")]
pub struct Workshop {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(widget = Date, label = "Date")]
    pub date: Option<String>,
    #[field(widget = Integer, label = "Seats", help = "How many people can join.")]
    pub seats: Option<i64>,
    #[field(widget = MediaPicker, label = "Photo")]
    pub photo: Option<i64>,
    #[field(widget = Markdown, label = "Description")]
    pub description: Option<String>,
}

impl rustango_cms::PageTypeOverrides for Workshop {}
```

What each part does:

| Part | What it does |
|---|---|
| `rustango::Model` + `#[rustango(table = …)]` | The page type's own table, `shop_workshop`. One row for each Workshop page. |
| `page_id` with `fk = "cms_page", unique` | Links the row to its page. Every page type table has this field. |
| `#[page_type(…)]` | The name in the admin, the template, and the icon (a [Material Symbols](https://fonts.google.com/icons) name). `type_name` is the identifier — other types use it in their parent and child rules. |
| `#[field(widget = …)]` | One box in the page editor. The label and help text are what editors see. |
| `impl PageTypeOverrides for Workshop {}` | Required. An empty impl keeps all the default behaviour. See the next part. |

A field that holds blocks uses `widget = Stream` and lists the blocks editors may use. The home page's introduction:

```rust
#[field(
    widget = Stream,
    label = "Introduction",
    help = "Welcome text, a photo, a quote from a customer.",
    allowed(heading, paragraph, image, quote)
)]
pub body: Option<String>,
```

In the template, a Stream field is already HTML: `{{ _stream_html['body'] | safe }}`.

### Make the table

The table is made by a migration. Never write the migration by hand — generate it:

```sh
cargo run --example ceramics_shop -- makemigrations
cargo run --example ceramics_shop -- migrate-tenants
```

### Keep the type in the binary

Page types register themselves when the program starts. In an example or a small binary, the linker can drop a type that no code uses. The shop's `main.rs` names each type once so this cannot happen:

```rust
let _ = std::any::type_name::<models::Workshop>();
```

## `PageTypeOverrides` — change how a page type behaves

Every method has a default, so you only write the ones you need.

| Method | Default | Use it to… |
|---|---|---|
| `allowed_parent_types()` | any | Allow this type only under some types, for example `&["ShopPage"]`. |
| `allowed_child_types()` | any | Allow only some types under this one. |
| `is_creatable()` | `true` | Hide the type from the **New page** list (for example a type used only once). |
| `workflow_slug()` | none | Send pages of this type through a review workflow. |
| `view_restriction()` | none | Show pages of this type only to signed-in members or some groups. |
| `children_query(pool, page)` | all live children | Choose which children the template gets in `children`. |
| `public_context(pool, page)` | nothing | Give the template extra data that is not a page or its fields. |
| `routes`, `route_context`, `route_outcome` | none | Extra URLs under a page, their data, and the template (or a 404) they answer with. See [Your own data in the admin](shop-dev-admin-screens.md). |
| `display_fields`, `extra_tabs` | — | Chips in the admin list, extra editor tabs. |

In the methods that get a page (`public_context`, `route_context`), `self` is that page's row of your table, so `self.seats` is the number the editor saved for this workshop. (When a page has no row yet, `self` is `Default::default()`.)

### `children_query` — only published children

By default a page's template gets all its live children in `children`, in the order editors arranged them. The shop page says so itself:

```rust
#[async_trait]
impl rustango_cms::PageTypeOverrides for ShopPage {
    async fn children_query(
        &self,
        pool: &Pool,
        page: &rustango_cms::Page,
    ) -> Result<Option<Vec<rustango_cms::Page>>, ExecError> {
        Ok(Some(rustango_cms::published_children(pool, page).await?))
    }
    // …
}
```

Return `Ok(None)` to keep the default. The home page uses the same code, so its cards show only published pages.

A page with hundreds of children should not list them all. `published_children_page` returns one window of them, in the order you choose, and the total:

```rust
let first = rustango_cms::published_children_page(
    pool, page, rustango_cms::ChildOrder::NewestFirst, 20, 0,
).await?;
Ok(Some(first.pages))
```

`first.total` is the number of live children, for "page 1 of 7" links. Serve the next windows from a route such as `page/(?P<n>\d+)` (see `routes` above), calling the same helper with an `offset`.

### Listing pages: `child.builder`

A listing shows fields of each child on its cards: the shop page shows each product's photo, glaze and price. Every child whose type was built in the admin carries its fields, so no code is needed:

| Variable | What it is |
|---|---|
| `child.builder.<key>` | The value the editor filled in, as stored (`Moon blue`, `120`, a media id). In another language, the translated text. |
| `child.builder_labels.<key>` | For a **select**, **radio** or **checkboxes** field: the option's label in the visitor's language (`Bleu lune`). A list for checkboxes. |

The product card (`_product_card.html`, shortened here):

```jinja
{% set d = child.builder | default(value=false) %}
<a href="{{ child | page_href }}">
    {% if d and d.photo %}
    <img src="{{ rcms_image_url(media_id=d.photo, filter="fill-800x800") }}" alt="{{ child.title }}" loading="lazy">
    {% endif %}
    <h3>{{ child.title }}</h3>
    {% if d and d.glaze %}<p>{{ child.builder_labels.glaze | default(value=d.glaze) }}</p>{% endif %}
    {% if d and d.price %}<p>€{{ d.price }}</p>{% endif %}
</a>
```

The page itself has the same pair: `builder.<key>` and `builder_labels.<key>`. The CMS loads the fields of all children at once — one query per page type, not one per child.

### `public_context` — extra data for the template

For data that is neither a page nor its fields — rows from your own table, a count, a feed — return it from `public_context`. Each key becomes a template variable:

```rust
async fn public_context(
    &self,
    pool: &Pool,
    page: &rustango_cms::Page,
) -> Result<serde_json::Map<String, serde_json::Value>, ExecError> {
    let mut ctx = serde_json::Map::new();
    ctx.insert("reviews".to_owned(), load_reviews(pool, page).await?);
    Ok(ctx)
}
```

> **Avoid N+1 queries.** Load the data for all items in one query — not one query per item in a loop.

A key named `page` is special: its keys are added to the page's own, so the template reads them as `page.<key>`. They never replace a key the page has already (`title`, `url_path`…). The shop page uses this for `page.stockist_count`.

## Inline panels: rows that belong to a page

Some data belongs to one page but has many rows: the speakers of a workshop, the causes of a campaign. An inline panel shows them in the page editor as a list of cards, each with its own fields, which editors add, remove and reorder. The rows live in your own table.

Three `PageTypeOverrides` methods make a panel:

```rust
#[async_trait]
impl rustango_cms::PageTypeOverrides for Workshop {
    async fn inline_panels(&self, pool: &Pool, page_id: i64) -> Result<Vec<InlinePanelSpec>, ExecError> {
        Ok(vec![InlinePanelSpec::with_widgets(
            "speakers",
            "Speakers",
            vec![
                Widget::new(WidgetKind::Select, "potter_id", "Potter").with_options(potters(pool).await?).required(),
                Widget::new(WidgetKind::Text, "topic", "Topic"),
                Widget::new(WidgetKind::Boolean, "is_host", "Hosts the class").with_variant("switch"),
            ],
        )
        .in_tab("Speakers")
        .with_row_label("Speaker")
        .with_icon("record_voice_over")])
    }

    async fn load_inline_panel(&self, pool: &Pool, page_id: i64, panel: &str) -> Result<Vec<Map<String, Value>>, ExecError> {
        // One map per row; keys are the widget names. Add `id` so saves can
        // tell existing rows from new ones, and `__title` for the card heading.
        load_speakers(pool, page_id).await
    }

    async fn save_inline_panel(&self, pool: &Pool, page_id: i64, panel: &str, rows: &[Map<String, Value>]) -> Result<(), ExecError> {
        // `rows` is every row the editor kept, in order: update the ones with
        // an `id`, insert the others, delete the stored rows not in the list.
        save_speakers(pool, page_id, rows).await
    }
}
```

| Part | What it does |
|---|---|
| `with_widgets(name, label, widgets)` | The fields of one row. Every widget kind works: selects, switches, the image and document choosers, checkbox lists. |
| `in_tab("Speakers")` | Shows the panel in its own editor tab. Panels with the same tab label share it. Without it, the panel is in the Content tab. |
| `with_row_label("Speaker")` | The add button reads "Add speaker"; a new card is called "Speaker" until a choice in it names it. |
| `allow(add, delete, order)` | Turn row operations off, for rows another system owns. Pair it with `Widget::read_only()` on the fields editors must not change. |

A row's values arrive as text, the way the browser posts them. An unchecked switch posts nothing, so a missing key means "off". A checkbox list posts a JSON array of the chosen values.

### Many rows: per-row panels

When a panel can hold hundreds of rows (every cause of a large organization), call `.per_row()`. The tab then lists only each row's title; a card's fields load when the editor opens it, the card has its own **Save** button, and add, remove and reorder take effect at once. The page's own Save sends nothing for the panel.

A per-row panel uses five smaller methods instead of `load_inline_panel` / `save_inline_panel`:

| Method | Returns |
|---|---|
| `inline_rows(pool, page_id, panel)` | Each row's id and title, in order (`InlineRowSummary`). Keep it cheap: it runs on every editor load. |
| `inline_row(pool, page_id, panel, row_id)` | One row's values by widget name; `row_id = None` gives a new row's starting values. `Ok(None)` if the id isn't one of this page's rows. |
| `save_inline_row(pool, page_id, panel, row_id, values)` | `InlineRowSaved::Saved { id, title }`, or `InlineRowSaved::Invalid(errors)` — each `FieldError` shows under its field (`"_form"` for the card). `row_id = None` adds the row. |
| `delete_inline_row(pool, page_id, panel, row_id)` | `true` when removed; `false` refuses. |
| `reorder_inline_rows(pool, page_id, panel, ids)` | Stores the order, first to last. Ignore ids that aren't this page's. |

Every one of these checks that the row belongs to `page_id`: the URLs carry ids an editor can change.

> **Name your fields carefully.** A page type's own fields share the form with the page editor's fields. `#[derive(PageType)]` refuses a `#[field]` named like one of them (`status`, `title`, `slug`, `tags`…). Rename the Rust field and keep the column with `#[rustango(column = "status")]`.

## Replace a built-in block

The CMS has built-in blocks (`heading`, `paragraph`, `image`, `quote`, `rich_text`, `gallery`…). To change how one works on your site, register your own block with the same `type_name`:

```rust
#[derive(Default)]
pub struct ShopQuote;

impl rustango_cms::Block for ShopQuote {
    fn type_name(&self) -> &'static str { "quote" }          // the built-in's name
    fn verbose_name(&self) -> &'static str { "Customer quote" }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::widget("text", "Quote", WidgetKind::Textarea),
            BlockField::widget("customer", "Customer", WidgetKind::Text),
        ]
    }
}
rustango_cms::register_block!(ShopQuote);
```

Your block wins: editors see it in the block menu, pages render with it, and the built-in one is not listed. Content saved with the old block keeps its values; give your fields the same names when you can. The server log says which block was replaced when the site starts.

Two blocks of your own with the same name is a mistake: the CMS keeps the first and logs an error.

## `LibraryTypeHandler` — reusable content

The **Library** holds content that editors write once and place on many pages. The shop adds an **Information** type for texts like "Shipping" or "Care instructions":

```rust
#[derive(Default)]
pub struct Information;

#[async_trait]
impl rustango_cms::library::LibraryTypeHandler for Information {
    fn app_label(&self) -> &'static str {
        "shop"
    }
    fn type_name(&self) -> &'static str {
        "Information"
    }
    fn verbose_name(&self) -> &'static str {
        "Information"
    }
    fn extension_table(&self) -> Option<&'static str> {
        None
    }
}

rustango_cms::register_library_type!(Information);
```

`extension_table()` returns `None` because a title and a body are enough — they are in the Library's own table. Return a table name when the type needs typed fields of its own.

Optional methods change the admin list and rights, for example `list_display()`, `search_fields()`, `list_filter()`, `default_ordering()`, `bulk_actions()`, `revisions_enabled()`, `preview_enabled()`, `can_edit()` and `can_delete()`. `render_template()` sets the template used when an element is placed on a page.

Editors place a Library element on a page with the **Snippet** block (its code name is `snippet_chooser`). The shop's **Content page** allows it in its body.

## `TaxonomyHandler` — a new vocabulary

The admin always has **Categories**. The shop adds a second vocabulary, **Clay** (stoneware, porcelain…):

```rust
#[derive(Default)]
pub struct Clay;

impl rustango_cms::category::TaxonomyHandler for Clay {
    fn slug(&self) -> &'static str {
        "clay"
    }
    fn verbose_name(&self) -> &'static str {
        "Clay"
    }
    fn hierarchical(&self) -> bool {
        false
    }
    fn icon(&self) -> Option<&'static str> {
        Some("texture")
    }
    fn description(&self) -> Option<&'static str> {
        Some("The clay a piece is made from.")
    }
}

rustango_cms::register_taxonomy!(Clay);
```

When the site starts, the vocabulary is saved to the database. It appears in **Taxonomies** next to **Categories**, and editors add its terms there.

| Method | Default | What it does |
|---|---|---|
| `slug()`, `verbose_name()` | required | The identifier and the name in the admin. |
| `hierarchical()` | `true` | `false` = a flat list, no parent terms. |
| `max_depth()` | `10` | How deep terms can be nested. |
| `icon()`, `description()` | none | Shown in the admin. |
| `ext_table()`, `ext_widgets()`, `save_ext()` | none | Extra typed fields for each term, in a table of your own (for example a flag and a code for a "Country" vocabulary). |

## `register_site_setting!` — a settings group

The **brand** group from [chapter 1](shop-brand.md) is defined in one macro call:

```rust
rustango_cms::register_site_setting!("brand", "Brand", || vec![
    Widget::new(WidgetKind::Text, "shop_name", "Shop name").required(),
    Widget::new(WidgetKind::Text, "tagline", "Tagline"),
    Widget::new(WidgetKind::Color, "accent", "Accent colour"),
    Widget::new(WidgetKind::Color, "background", "Background colour"),
    Widget::new(WidgetKind::Textarea, "footer", "Footer text"),
]);
```

- The first argument is the scope. Templates read it with `site_setting(scope="brand")`.
- The second is the name in the admin.
- The list of widgets gives the boxes. `.required()` makes a box required.

The group is shown as **TYPED** in **Site settings**, and editors get a normal form instead of raw JSON. How the shop reads the values is in [Show menus and settings in templates](shop-dev-templates.md).

## Check it worked

Start the shop and sign in to the admin:

- **Page types** lists **Workshop**, **Home page**, **Shop page** and **Content page**.
- **Library** has the **Information** type.
- **Taxonomies** lists **Categories** and **Clay**.
- **Site settings** has the **brand** row with **TYPED**.

## If something goes wrong

| Problem | What to do |
|---|---|
| A type is missing in the admin. | Name the type once in `main` (see "Keep the type in the binary"), rebuild and restart. |
| `no such table: shop_workshop` | Run `makemigrations` and `migrate-tenants`. |
| The compiler says `PageTypeOverrides` is not implemented for your type. | Add `impl rustango_cms::PageTypeOverrides for YourType {}`. |

## Next

Back to [the list of chapters](shop-overview.md).
