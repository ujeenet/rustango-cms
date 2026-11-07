# Starter theme pictures

The sample pictures the starter themes show (`crates/rcms-scaffold/themes/_shared/media/*.jpg`) are drawn by `generate.html` in this folder: original illustrations, not photos from elsewhere, so they ship with no licence to track. Each scene uses a fixed random seed, so drawing it again gives the same picture.

## Draw them again

1. Serve this folder: `python3 -m http.server 8411 --directory scripts/theme-media`.
2. Open `http://127.0.0.1:8411/generate.html?scene=<name>` in a browser at a 1600×1000 window to see one. The scene names are the keys of `SCENES` in `generate.html`.
3. Save every scene as a JPEG with Playwright (quality 80):

   ```js
   await page.setViewportSize({ width: 1600, height: 1000 });
   await page.goto("http://127.0.0.1:8411/generate.html");
   for (const scene of await page.evaluate(() => window.SCENES)) {
     await page.goto(`http://127.0.0.1:8411/generate.html?scene=${scene}`);
     await page.locator("canvas").screenshot({
       path: `crates/rcms-scaffold/themes/_shared/media/${scene}.jpg`,
       type: "jpeg",
       quality: 80,
     });
   }
   ```

## Add a picture

1. Add a scene to `SCENES` in `generate.html` and save it as above.
2. Describe it in `crates/rcms-scaffold/themes/_shared/media.json`: `key`, `file`, `title`, and `alt` (what it shows, for people who can't see it).
3. Use it in a theme's `theme.json` as `"@media:<key>"`, in a page's data or a block's image field.

`rcms new` copies only the pictures a theme uses into the new project (`theme-media/`) and builds them into its binary; the first start puts them in the site's media library.
