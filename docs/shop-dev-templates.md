# Show menus and settings in templates

**Goal:** show the editors' menus and settings on the site, the way the Clay & Kiln shop does it.

**Who this is for:** developers who write the site's Tera templates.

**Time:** about 15 minutes.

This is a developer chapter of [Build a ceramics shop, step by step](shop-overview.md). All code is from the example shop in `examples/ceramics_shop/templates/_base.html`.

## Before you start

- You know the basics of [Tera](https://keats.github.io/tera/docs/) templates.
- The editors did the admin chapters, or you have the example shop running. How to start it is written at the top of `examples/ceramics_shop/main.rs`.

## The three helpers

Editors change menus and settings in the admin. Templates read them with three functions. They work in every template of a public page, including the layout it extends.

| Helper | Returns | Made in the admin at |
|---|---|---|
| `site_setting(scope="…")` | The values of one settings group, as an object. | **Site settings** |
| `menu(slug="…")` | One menu, with nested items. `null` if there is no menu with this slug. | **Navigation** |
| `auto_menu(parent_id=…, depth=…)` | Published pages with **Show in menus** ticked, as a tree. No menu to keep up to date. | **Pages** (the **Promote** tab) |

The helpers read data that was loaded once before the page is rendered. A menu with ten items does not make ten database queries.

## `site_setting` — the shop name and colours

The editors typed the name, colours and footer text in [chapter 1](shop-brand.md). The layout reads them once, at the top:

```jinja
{% set brand = site_setting(scope="brand") %}
{% set shop_name = brand.shop_name | default(value="Clay & Kiln") %}
```

Then it uses them anywhere:

```jinja
<style>
    :root {
        --accent: {{ brand.accent | default(value="#9c5431") }};
        --bg: {{ brand.background | default(value="#faf6f0") }};
    }
</style>
...
<a href="/">{{ shop_name }}</a>
...
{% if brand.footer %}<p>{{ brand.footer }}</p>{% endif %}
```

> **Always give a default.** Before an editor saves the group, `site_setting` returns `null`. With `default(value=…)` the page still looks right.
>
> **Watch out for empty strings.** `default` only works when the value is missing. An editor can save an empty box, and then the value is `""`. For text you show, test it with `{% if brand.footer %}`.

The boxes of the **brand** group are defined in code. See [The helper traits](shop-dev-helper-traits.md).

## `menu` — the menu the editors built

The editors built the `main` menu in [chapter 8](shop-menu.md). The shop shows it in the header:

```jinja
{% set nav = menu(slug="main") %}
{% if nav %}
<nav aria-label="Main">
    {% for item in nav.items %}{% if item.url %}
    <a href="{{ item.url }}"
       class="{% if item.in_active_trail %}font-semibold text-[color:var(--accent)]{% endif %}">
        {{ item.label }}
    </a>
    {% endif %}{% endfor %}
</nav>
{% endif %}
```

`menu()` returns an object with `slug`, `name` and `items`. Each item has:

| Field | What it is |
|---|---|
| `label` | The text to show. For a page item with no label, the page title. In another language, the translated label. |
| `url` | Where the link goes. `null` for an item with no page and no URL — a heading for a group of sub-items. |
| `is_page`, `page_id` | `true` and the page id when the item links to a page. |
| `is_active` | The item links to the page that is open now. |
| `in_active_trail` | The item links to the open page or to one of its parents, or one of its sub-items is active. The shop uses it to highlight **Shop** on product pages. |
| `open_in_new_tab` | Add `target="_blank"` when it is `true`. |
| `children` | The sub-items, with the same fields. |

The example shop shows only the first level. To show sub-items too, loop over `children`:

```jinja
{% for item in nav.items %}
<li>
    {% if item.url %}<a href="{{ item.url }}">{{ item.label }}</a>{% else %}<span>{{ item.label }}</span>{% endif %}
    {% if item.children | length > 0 %}
    <ul>
        {% for child in item.children %}
        <li><a href="{{ child.url }}"{% if child.is_active %} aria-current="page"{% endif %}>{{ child.label }}</a></li>
        {% endfor %}
    </ul>
    {% endif %}
</li>
{% endfor %}
```

> **Test for `null`.** When there is no menu with the slug, `menu()` returns `null`, not an error. Wrap the markup in `{% if nav %}` so a new site without menus still renders.
>
> **Private pages stay private.** Items that link to pages the visitor is not allowed to see are left out.

### A second menu

Editors can make more menus, for example `footer`. Show it the same way with `menu(slug="footer")`. Tell the editors which slugs your design uses — they must type the same slug.

## `auto_menu` — the page tree as a menu

Sometimes nobody wants to keep a menu up to date. `auto_menu()` builds one from the pages: every published page with **Show in menus** ticked on its **Promote** tab.

The shop's footer lists the pages under Home:

```jinja
{% for root in auto_menu(depth=2) %}{% if root.children | length > 0 %}
<nav aria-label="Explore">
    {% for item in root.children %}
    <a href="{{ item.url_path }}">{{ item.title }}</a>
    {% endfor %}
</nav>
{% endif %}{% endfor %}
```

Arguments:

| Argument | What it does |
|---|---|
| `parent_id` | Start below this page. Leave it out for the top pages (in the shop: only **Home**). |
| `depth` | How many levels. `1` = only the pages at the start; `2` = also their children. The default is `1`. |

Each item has `page_id`, `title`, `url_path` and `children`.

In the shop, `auto_menu(depth=2)` returns **Home**, and Home's `children` are the pages under it — today only **Shop**. When the editors add an **About us** page, it appears in the footer by itself.

## Which one should I use?

| You want… | Use |
|---|---|
| Editors decide the items, the order and the labels, and add links to other websites. | `menu(slug=…)` |
| The menu always follows the page tree. | `auto_menu(…)` |
| A menu that starts from the page tree, and editors change it later. | `menu(slug=…)` — the editors click **Generate from menu pages** once. |

## `rcms_meta_tags` — titles and share images for Google

One call in the `<head>` emits the description, the canonical link, and the Open Graph and Twitter tags from the page's **Promote** tab:

```jinja
{{ rcms_meta_tags(page=page, canonical=canonical_url, origin=site_origin) | safe }}
```

- `canonical_url` and `site_origin` are set by the renderer. With `origin`, the canonical and image URLs are absolute (`https://your-shop/...`) — social networks ignore a relative `og:image`.
- When the editor chose no share image, the renderer fills in the page's first photo (a media-picker field, or the first image block). Each child in `children` gets the same fallback in `child.og_image_media_id`, so cards on a listing always have a picture when the page has one.
- The `<title>` is yours to write; the shop uses the SEO title when there is one:

```jinja
<title>{% if page.seo_title %}{{ page.seo_title }}{% else %}{{ page.title }}{% endif %} · {{ shop_name }}</title>
```

See [Be found on Google](shop-seo.md) for the editor's side.

## If something goes wrong

| Problem | What to do |
|---|---|
| `menu()` returns nothing. | Check that the slug in the template and in **Navigation** is the same, and that the menu was saved. |
| `auto_menu()` misses a page. | The page is a draft, or **Show in menus** is not ticked. |
| A new setting value does not show. | Check the scope name in `site_setting(scope=…)`. Scopes are case-sensitive. |
| `Function 'menu' not found`. | The helpers are not registered on your `Tera`. Call `rustango_cms::admin::register_templates(&mut tera)` before you build the routers, as the shop's `main.rs` does. |
| A helper returns nothing in a template that is not a page. | The helpers read data the page renderer loads for each page. They are for templates of public pages. |

## Next

[The helper traits](shop-dev-helper-traits.md) — how the shop's code-made page types, library type, vocabulary and settings work.
