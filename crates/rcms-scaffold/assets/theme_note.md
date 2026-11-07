
## Your theme

This site was made with the **__THEME__** starter theme. On the first start
(`cargo run -- quickstart`), `theme.json` created the page types, sample
pages, header and footer menus, site name and colours. It never runs again
for that site, so everything from here on happens in the CMS admin:

- **Pages:** rewrite or delete the sample pages, add your own.
- **Page types:** add or change fields, and the blocks editors can use in
  the **Content** field. No Rust code involved.
- **Navigation:** the `header` and `footer` menus.
- **Site settings → Site:** the site name, tagline and footer text.
- **Colours and fonts:** the theme named "Starter: …" in the Rustango
  admin at `/admin` → Themes.

The look lives in `templates/`: `_base.html` (header, footer, menus),
`_style.html` and `_theme.css.html` (styles), one template per page type,
and `blocks/` for the theme's own content blocks.

To reuse this site's look elsewhere, `cargo run -- theme pack mysite.zip`
makes a theme package, and `cargo run -- theme install mysite.zip` puts it
on another site.
