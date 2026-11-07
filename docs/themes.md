# Themes

A theme is how a Rustango-CMS site looks: its templates, its colours and fonts, and optionally the page types, sample pages, menus and site settings that go with them. This guide covers:

1. [Starter themes](#starter-themes): a finished-looking new site in two commands.
2. [Theme packages](#theme-packages): take a site's look to another site, from the admin or the command line.
3. [Data page types and code page types](#data-page-types-and-code-page-types): what a theme can and can't change, depending on how a site defines its pages.
4. [Templates reference](#templates-reference) and [writing your own theme](#write-your-own-theme).
5. [Troubleshooting](#troubleshooting).

## Starter themes

A starter theme gives a new site a finished look on its first start: page types, sample pages, header and footer menus, a site name, and colours and fonts. Everything it creates is ordinary content you edit in the admin. There are no Rust models to write.

| Theme | What you get |
|---|---|
| `blog` | A home page, a blog with dated posts, About and Contact |
| `news` | Sections (World, Technology, Culture) with articles, a featured story per section, About and Contact |
| `portfolio` | A home page, projects with client, year and services, About and Contact |
| `blog-portfolio` | Projects and a blog side by side, for a personal site |

### Start a site from a theme

```sh
cargo install rcms-scaffold   # once
rcms new mysite --dir ~/projects --theme portfolio
cd ~/projects/mysite
cargo run -- quickstart
```

The commands are the same in bash, zsh and Windows PowerShell. On Windows, use `--dir $HOME\projects`.

`quickstart` creates the database, a site called `demo` and an `admin` user, then starts the server. On that first start the theme fills the empty site. Open <http://demo.localhost:8080/> to see it, and <http://demo.localhost:8080/cms-admin/> to edit it.

The theme is applied once per site, and only to a site with no pages. It never overwrites what an editor has done.

### What to edit where

| To change | Go to |
|---|---|
| Text, images, sample pages | **Pages** in the CMS admin |
| The fields of a page type, and which blocks the **Content** field offers | **Page types** in the CMS admin |
| The header and footer menus | **Navigation** in the CMS admin |
| What visitors see on a missing page or an error | **Pages** in the CMS admin: the **Error page** pages under the home page |
| The site name, logo and browser icon | **Settings → Branding** in the CMS admin (the same ones the admin shows) |
| The tagline, footer text and footer badge | **Site settings → Site** in the CMS admin |
| Colours, fonts, corner radius | The Rustango admin at `/admin` → **Themes**, the theme named "Starter: …" |
| One template, for this site only | **Templates** in the CMS admin |
| The layout for every site of the project | The files in `templates/` |

### Sample pictures

A starter theme fills its sample pages with pictures: project and post covers, the About page's hero, media-and-text blocks and cards, a slider or a gallery. They are original illustrations made for Rustango-CMS, free to keep or replace. On the first start they go into the site's media library (**Media** in the CMS admin) like any upload; replace one by picking another picture in the page, or delete it from **Media** once nothing uses it.

The site's logo, browser icon and footer badge start as the Rustango-CMS logo, also in the media library. Pick your own logo and icon under **Settings → Branding**, and the footer badge under **Site settings → Site**.

The project keeps them in `theme-media/` and builds them into its binary, so a deployed site needs no extra folder.

### Error pages

Every starter theme comes with four error pages, in its own layout and wording:

| Status | When visitors see it | Address of the page itself |
|---|---|---|
| 404 | The address doesn't match any page | `/page-not-found` |
| 403 | The page exists but they may not see it | `/access-denied` |
| 500 | The server failed to show the page | `/server-error` |
| 503 | The site is down for maintenance | `/maintenance` |

They are ordinary pages of the **Error page** type under the home page, so you change their title and text in **Pages** like any other page. The visitor still gets the real error status, not `200`. Error pages stay out of menus, out of the sitemap, and ask search engines not to index them. The `error_page.html` template shows the status, the title, the page content and a link back to the home page, inside the site's header and footer.

If you delete one, that status falls back to a plain built-in page.

### Content blocks

Every page type with body content has a **Content** field. Editors add blocks to it in any order:

| Block | What it shows | Options |
|---|---|---|
| **Rich text** | Paragraphs, headings, lists and links | |
| **Image** | One image with a caption | |
| **Quote** | A quotation with an attribution | |
| **Embed** | A video or other media, by URL | |
| **Hero image** | A full-width picture with a heading and text over it | A button; medium or tall. Without a picture, the theme colour fills it |
| **Jumbo box** | One large message on a coloured box | A button; accent colour, soft or dark |
| **Media and text** | An image beside a heading and text | Image on the left or the right; a button; an image description |
| **Cards** | A grid of cards, each with an image, a title, text and a link | 2, 3 or 4 per row; add up to 12 cards |
| **Slider** | Slides that scroll sideways, each with an image, a heading, text and a button | One large slide at a time, or a row of several; up to 20 slides |
| **Gallery** | Pictures in a grid, each opening full size | 2, 3 or 4 per row; a caption per picture and one for the gallery |
| **Call to action** | A heading, text and a button | |

**Cards**, **Slider** and **Gallery** hold a list: add items with **Add card**, **Add slide** or **Add picture**, and reorder or remove them with the buttons on each item. A card without link text makes its title the link.

A button or link shows only when it has both a label and a link. A link is a page of the site (`/contact`) or a full address (`https://…`, `mailto:…`, `tel:…`); anything else is left out. **Hero image** uses the whole window width; **Jumbo box**, **Media and text**, **Cards**, **Slider** and **Gallery** the whole page width; the other blocks the reading column. On a phone, cards and pictures stack into fewer columns and **Media and text** puts the image above the text.

A slider scrolls by swipe, trackpad or keyboard, and the theme adds previous and next buttons below it.

**Cards**, **Slider** and **Gallery** are part of Rustango-CMS, so any site can use them. Their HTML has `rcms-block-*` classes and no styles of its own; the theme's `templates/_style.html` lays them out. **Hero image**, **Jumbo box**, **Media and text** and **Call to action** belong to the theme: they are defined once in `themes/_shared/blocks.json` of the scaffolder and added to every page type with a **Content** field, and their HTML is in `templates/blocks/<key>.html`. To add another block for editors, add it to the page type under **Page types**, then add a `templates/blocks/<key>.html` for its HTML.

### The project's files

| File | What it is |
|---|---|
| `theme.json` | What the theme creates on first start: page types, pages, menus, site settings, colours |
| `src/theme.rs` | Applies `theme.json` and registers the **Site** settings form |
| `templates/` | The project's templates, shared by every site |
| `templates/_base.html` | Header, menus and footer, shared by every page |
| `templates/_style.html` | The shared styles; every colour, font and radius is a theme variable |
| `templates/_theme.css.html` | The theme's own styles |
| `templates/<type>.html` | One template per page type, such as `post.html` or `project.html` |
| `templates/blocks/` | The HTML for the theme's own content blocks |
| `templates_tenants/<site>/` | A site's own templates, which win over `templates/` for that site. The admin's **Templates** editor and **Site theme** screen write here. |

## Theme packages

A theme package is one `.zip` file. It holds a site's templates and a `theme.json` with its page types, colours and site settings, and optionally its pages and menus. Make one from a site, then install it on another site to give it the same look.

### What's in a package

| | In the package? |
|---|---|
| Templates | **Yes**, every template the site renders with |
| Colours and fonts | **Yes**, the site's default theme row |
| Site settings (name, tagline, footer) | **Yes** |
| Page types made in the admin, or by a starter theme | **Yes**, with their fields and content blocks |
| Page types defined in Rust code | **No**: only their templates. They exist wherever that code runs. |
| Pages and menus | **Only if you ask for them** ("Include pages and menus", or `theme pack` without `--no-pages`), and only pages of the page types above |
| Pages of code page types, and menu links to them | **No** |
| Images and documents | **No**. The text travels; image fields arrive empty. |
| Users, roles, form submissions, analytics | **No**. A package is a look, not a backup. |

### From the admin: Settings → Site theme

**Site theme** is in the CMS admin's sidebar under **Settings**, for superusers.

- **Current look** shows the installed theme, the site's colours, and how many templates are this site's own.
- **Download as a theme** downloads the package. Tick **Include pages and menus** to add the content.
- **Install a theme**: upload a `.zip`, check the preview, then click **Install theme**. The preview lists:
  - whether the theme will create pages (empty site) or only change the look (site with content);
  - each page type it brings: **New**, **Uses the theme's template**, or **Kept as defined in code**;
  - each template it brings: **New** or **Replaces**;
  - any page type in use whose template would be missing afterwards.
- **Restore previous look** swaps back to the templates and colours from before the last install. Click it again to swap forward.

The admin installs the templates into **this site's own** template folder (`templates_tenants/<site>/`). They take effect within a few seconds, with no restart, and other sites on the same server keep their look.

### From the command line

```sh
cargo run -- theme pack mysite.zip               # the look, plus pages, menus and settings
cargo run -- theme pack mylook.zip --no-pages    # the look only
cargo run -- theme check mysite.zip              # is it a valid package?
cargo run -- theme install mysite.zip            # install it
```

Add `--site <name>` when the project serves more than one site.

The command line works on the **project's** `templates/` folder, not on a site's own templates:

- `theme pack` packs `templates/`. Templates a site customised in the admin are not included; use the admin's download for those.
- `theme install` writes into `templates/`, so the new look applies to **every site** of the project. It first copies the folder to `templates.bak-<date>`. Restart the server afterwards to load the new templates.

Use the admin to change one site; use the command line to change a project, for example in a deploy script.

### What installing does

| The site you install on | What happens |
|---|---|
| **Empty** (no pages yet) | Everything in the package is created: page types, pages, menus, site settings and colours, exactly like a starter theme. |
| **Has content** | Only the look changes. Page types the site is missing are added. Page types made in the admin, or by a theme, render with the package's templates; their fields and content are untouched. Page types defined in code keep the template their code names. The package's colours become the site's default. The package's error pages are added for any status the site has no error page for; the site's own error pages stay. Other pages, menus and site settings stay as they are. |

Templates are merged, never deleted. The package's files replace files of the same name, and every other file stays, so pages of a type the package doesn't know keep their template.

## Data page types and code page types

A Rustango-CMS site defines its page types in one of two ways, and themes treat them differently.

| | Data page types | Code page types |
|---|---|---|
| Defined | In the admin under **Page types**, or by a starter theme | In Rust, with `#[derive(PageType)]` or a page-type handler |
| Fields reach templates as | `builder.<field>` | `extension.<field>` |
| A theme package carries | The type, its fields, its template, and optionally its pages | Only its template |
| Installing on a site that doesn't have the type | Creates the type | Not possible: the type needs the Rust code |

### Sites built on data page types

Starter themes and sites built in the admin use data page types. Every theme works on every such site: the four starter themes share the same page types (Home, Page, Blog, Post, Section, Article, Projects, Project), so switching between them keeps all content.

### Sites built on code page types

Sites whose page types are Rust models (for example a project made with `rcms new --template blog`) can share a look **between sites of the same project**, which run the same code:

1. On the source site, customise templates in **Templates**, then **Site theme → Download as a theme**.
2. On the target site, **Site theme → Install a theme**.
3. The target's pages now render with the source's templates, with their own content. A code page type uses the template its code names, such as `article_page.html`, and the package's file of that name is the one installed.

Pages of code page types don't travel in a package, so create them on the target site, or copy the site with a backup.

A theme made for data page types, such as a starter theme, changes a code-type site only where its templates are used. A template read through `extension.<field>` won't fit a theme template that reads `builder.<field>`, and a page whose template doesn't extend the theme's `_base.html` keeps its old look.

### Templates that depend on site data

A template that names something stored on the source site only shows it on a target that has the same thing:

- a snippet: `cms_snippet(slug="promo")`;
- a menu: `menu(slug="header")`;
- a site-settings scope: `site_setting(scope="site")`;
- a fixed image id: `rcms_image_url(media_id=12, …)`.

Starter themes use the `header` and `footer` menus and the `site` settings scope, which their installs create.

### See which template renders what

An admin can turn on **Preferences → Templates on the site → Show templates on the site** in the CMS admin. While it is on and you browse the public site (signed in), the **Templates** button in the admin bar (bottom right) opens a panel that lists the page's template, the layout it extends and every block template on the page, each with where it comes from:

| Source | Meaning | Change it |
|---|---|---|
| This site's copy | A template customised for this site | **Templates** in the CMS admin (the **Edit** link) |
| Project templates/ folder | The project's own file in `templates/` | **Templates** (customise it for this site) or the file itself |
| Built into Rustango-CMS | A template that ships with the CMS, such as **Cards**, **Slider** or **Gallery** | Style it from `templates/_style.html` |

Hovering a block on the page outlines it and names its template. Only you see this; visitors never do, and pages shown with it are not cached.

## Templates reference

What a page's template receives:

| Name | What it is |
|---|---|
| `page` | The page: `title`, `url_path`, `seo_title`, `seo_description`, `depth`, … |
| `builder` | The fields of a data page type, for example `builder.excerpt` |
| `extension` | The fields of a code page type, for example `extension.body_markdown` |
| `children` | The page's live child pages, each with its own `builder` values |
| `ancestors` | The pages above this one, root first |
| `theme_css` | The site's colours and fonts as CSS variables: `{{ theme_css \| safe }}` |
| `theme_font_url` | The stylesheet URL for the theme's web fonts, if any |
| `theme_default_mode` | `light`, `dark` or `system` |
| `LANG`, `DIR` | The page's language and text direction |

And the functions it can call:

| Function | What it returns |
|---|---|
| `stream_render(name="body")` | A **Content** field rendered as HTML |
| `menu(slug="header")` | A menu from **Navigation**: `items`, each with `label`, `url`, `is_active`, `children` |
| `auto_menu(parent_id=…, depth=1)` | A page's children that are shown in menus |
| `site_setting(scope="site")` | A settings group, for example `.name` |
| `language_switcher()` | The page in each language: `code`, `url`, `is_current` |
| `rcms_image_url(media_id=…, filter="width-800")` | An image URL at a given size |

The theme's colour variables are `--color-bg`, `--color-surface`, `--color-text-primary`, `--color-text-secondary`, `--color-text-muted`, `--color-border`, `--color-link`, `--font-heading`, `--font-body`, `--radius-sm`, `--radius-md`, `--radius-lg`, `--shadow-sm`, `--shadow-md` and `--shadow-lg`.

## Write your own theme

Any site can become a theme: download it from **Site theme** and you have a package. To write one by hand, a theme is a `theme.json` plus templates:

```json
{
  "name": "My theme",
  "site": { "tagline": "…", "footer": "…" },
  "branding": { "site_name": "My site" },
  "theme": { "base": "paper", "slug": "my-theme", "name": "My theme",
             "tokens": { "color_link": "#2f6fdd", "font_heading": "Georgia, serif" } },
  "page_types": [ { "type_name": "Page", "verbose_name": "Page", "template": "page.html",
                    "parents": [], "children": ["Page"], "schema": { "nodes": [] } } ],
  "pages": [ { "key": "home", "type": "Page", "title": "Home", "slug": "" } ],
  "menus": [ { "slug": "header", "name": "Header", "items": [ { "page": "home" } ] } ],
  "error_pages": [ { "status": 404, "title": "Page not found", "slug": "page-not-found",
                     "body": [ { "type": "paragraph", "value": { "body": "Nothing here. Try the home page." } } ] } ]
}
```

| Field | What it holds |
|---|---|
| `name` | The theme's name, shown in **Site theme** |
| `site` | The `site` settings: `tagline`, `footer`, `footer_logo` |
| `branding` | Settings → Branding, where the site has none yet: `site_name`, `logo`, `favicon` (pictures as `"@media:<key>"`) |
| `theme.base` | A built-in colour preset to start from: `paper`, `ink`, `slate`, `forest`, `sunset`, `lumina`, `aurora-violet` or `editor-classic` |
| `theme.tokens` | Any colour, font, radius or shadow setting of the preset to override, by name (`color_link`, `font_heading`, `font_url`, `radius_md`, …) |
| `page_types[].schema` | A page-builder schema, the same document the **Page types** builder edits |
| `page_types[].parents`, `children` | Which page types this one may sit under, and hold |
| `pages` | One root page, the home page (`"slug": ""`), with every other page nested in `children` |
| `pages[].key` | A name menus use to point at the page |
| `pages[].data` | The page's field values. A **Content** field is a list of blocks: `{ "type": "rich_text", "value": { "body": "<p>…</p>" } }` |
| `pages[].in_menu` | Show the page in automatic menus |
| `menus[].items` | Links: `{ "page": "<key>" }`, `{ "label": "…", "url": "https://…" }`, or a heading `{ "label": "…", "children": [ … ] }` |
| `media` | Pictures the content uses: `key`, `file` (a plain file name), `title` and `alt`. Page data and block values point at one with `"@media:<key>"`; [`starter::apply_with_media`](https://docs.rs/rustango-cms/latest/rustango_cms/starter/fn.apply_with_media.html) stores the supplied files in the media library and puts their ids in place. A picture that isn't supplied leaves the field empty |
| `error_pages` | Pages shown for an error: `status` (404, 403, 500 or 503, each once), `title`, `slug` (default `error-<status>`) and `body`, a list of `heading`, `paragraph`, `image`, `quote`, `embed`, `code` or `raw_html` blocks. They are created under the home page and render with `error_page.html` |

To check a package or a `theme.json` without a database, run `cargo run -- theme check <file>`, or call `rustango_cms::starter::parse` on its contents.

To make a package by hand, zip the `theme.json` at the top level with the templates under a `templates/` folder.

## Limits and safety

- **Only superusers** can download or install in the admin. A theme's templates run on the server and can call every template function the site has, so only install packages from people you trust.
- A package may hold at most 500 files and 8 MB unpacked, and only text files. The upload limit is 10 MB.
- Package paths can't leave the template folder: names with `..` or absolute paths are refused.
- Every template in a package is checked before any is written. If one has an error, nothing is installed.

## Troubleshooting

| You see | Why, and what to do |
|---|---|
| **Site theme** says per-site templates are not configured | The project doesn't set up per-site templates in `src/main.rs`. Projects made with `rcms new` 0.4 or later do. In an older project, build a `TenantTemplates`, `install` it and pass it to `PublicRouter::tenant_templates_shared`; see a newly generated `main.rs`. |
| A page answers with an error after an install | Its page type uses a template the site doesn't have. The preview (and `theme install`) list such page types; restore the previous look, or add the missing template in **Templates**. |
| Some pages don't change look | Their templates don't extend the theme's `_base.html`, or they belong to a code page type whose template the package doesn't replace. See [Sites built on code page types](#sites-built-on-code-page-types). |
| Images are missing after an install | Images are not part of a package. Pick them again in the pages' image fields. |
| `theme install` worked but nothing changed | The command line writes the project's `templates/`. Restart the server. A site with its own templates in `templates_tenants/<site>/` keeps them, because a site's own templates win. |
| The upload says the package can't be installed | The preview names the problem: a missing `theme.json`, a file outside `templates/`, an invalid page-type schema, or too large an archive. |
