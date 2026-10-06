# The helper traits

**Goal:** understand the parts of the Clay & Kiln shop that are made in Rust, and make your own.

**Who this is for:** Rust developers who build a site with rustango-cms.

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
| `children_query(pool, page)` | all children | Choose which children the template gets in `children`. |
| `public_context(pool, page)` | nothing | Give the template extra data that is not a page or its fields. |
| `display_fields`, `extra_tabs`, `routes`, `route_context` | — | Chips in the admin list, extra editor tabs, extra URLs under a page. |

### `children_query` — only published children

By default a page's template gets all its children in `children`, drafts too. The shop page wants only the published products:

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
