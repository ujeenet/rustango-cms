// A headless storefront for the Clay & Kiln shop: no framework, no build
// step. Every word and photo comes from the CMS's JSON API; this file only
// decides how it looks.

// Where the CMS runs — change the port if your shop uses another one. The
// CMS must list this page's origin in RCMS_API_CORS_ORIGINS, or the browser
// blocks every request.
const CMS = "http://shop.localhost:8080";

// The page to show comes from the address: ?path=/shop/moon-jar.
// A preview link from the CMS adds &id=…&token=… for a draft.
const params = new URLSearchParams(location.search);
const path = params.get("path") || "/";
const previewId = params.get("id");
const previewToken = params.get("token");

async function api(url) {
  const res = await fetch(CMS + url);
  if (!res.ok) {
    throw new Error(`${res.status} for ${url}`);
  }
  return res.json();
}

// The page itself. find/ answers with a redirect to the page's JSON, and
// fetch follows it. A draft is only readable with its preview token.
function loadPage() {
  if (previewId && previewToken) {
    const token = encodeURIComponent(previewToken);
    return api(`/api/v2/pages/${previewId}/?preview_token=${token}`);
  }
  return api(`/api/v2/pages/find/?html_path=${encodeURIComponent(path)}`);
}

// Photos are stored once; the CMS makes sized copies ("renditions").
const images = new Map();
async function photo(id, size = "medium") {
  if (!id) {
    return "";
  }
  if (!images.has(id)) {
    images.set(id, api(`/api/v2/images/${id}/`));
  }
  const img = await images.get(id);
  const alt = escape(img.alt_text || "");
  return `<img src="${CMS}${img.renditions[size]}" alt="${alt}" width="${img.width}" height="${img.height}">`;
}

// CMS paths become links inside this app.
function link(url, text) {
  return `<a href="?path=${encodeURIComponent(url)}">${escape(text)}</a>`;
}

function escape(text) {
  return String(text).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

// Paragraph and snippet text is Markdown. This handles what the shop uses —
// paragraphs, **bold**, *italics* and [links](…); a real site would use a
// Markdown library.
function markdown(text) {
  return String(text || "")
    .split(/\r?\n\s*\r?\n/)
    .map((para) => {
      const html = escape(para.trim())
        .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
        .replace(/\*(.+?)\*/g, "<em>$1</em>")
        .replace(/\[(.+?)\]\((.+?)\)/g, '<a href="$2">$1</a>');
      return html ? `<p>${html}</p>` : "";
    })
    .join("");
}

// A Stream field is a list of blocks: { type, id, value }. Draw the types
// this site uses and skip the rest.
async function blocks(list) {
  const parts = await Promise.all((list || []).map(async (block) => {
    const v = block.value || {};
    switch (block.type) {
      case "heading": {
        const level = ["2", "3", "4"].includes(String(v.level)) ? v.level : "2";
        return `<h${level}>${escape(v.text)}</h${level}>`;
      }
      case "paragraph":
        return markdown(v.body);
      case "quote": {
        const by = v.attribution ? `<footer>— ${escape(v.attribution)}</footer>` : "";
        return `<blockquote>${markdown(v.text)}${by}</blockquote>`;
      }
      case "image":
        return `<figure>${await photo(v.media_id, "large")}</figure>`;
      case "snippet_chooser": {
        const snippet = await api(`/api/v2/snippets/${v.snippet_id}/`);
        return `<aside class="note"><h3>${escape(snippet.title)}</h3>${markdown(snippet.body_markdown)}</aside>`;
      }
      default:
        return "";
    }
  }));
  return parts.join("");
}

// One function per page type, chosen by meta.type — the type's identifier
// ("type name" in the admin).
const templates = {
  async HomePage(page) {
    const e = page.extension || {};
    return `
      <section class="hero">
        ${await photo(e.hero_image, "large")}
        <div><h1>${escape(e.hero_heading || page.title)}</h1><p>${escape(e.hero_text || "")}</p></div>
      </section>
      <div class="prose">${await blocks(e.body)}</div>`;
  },

  // The children list carries titles and URLs only, so each product's
  // photo and price come from its own detail request.
  async ShopPage(page) {
    const e = page.extension || {};
    const products = await Promise.all(page.children.map((c) => api(c.detail_url)));
    const cards = await Promise.all(products.map(async (p) => `
      <li class="card">
        ${await photo(p.builder && p.builder.photo)}
        <h3>${link(p.url, p.title)}</h3>
        <p class="price">${price(p.builder && p.builder.price)}</p>
      </li>`));
    return `<h1>${escape(page.title)}</h1><p class="lead">${escape(e.intro || "")}</p><ul class="grid">${cards.join("")}</ul>`;
  },

  // Product is built in the admin, so its fields are under "builder".
  // The description is rich text: HTML the CMS has already cleaned.
  async product(page) {
    const b = page.builder || {};
    return `
      <article class="product">
        ${await photo(b.photo, "large")}
        <div>
          <h1>${escape(page.title)}</h1>
          <p class="price">${price(b.price)}</p>
          ${b.glaze ? `<p class="glaze">Glaze: ${escape(b.glaze)}</p>` : ""}
          <div class="prose">${b.description || ""}</div>
          <p>${link("/shop", "← Back to the shop")}</p>
        </div>
      </article>`;
  },
};

// Any other page type: its title and Stream field, if it has one.
async function fallback(page) {
  const e = page.extension || {};
  return `<h1>${escape(page.title)}</h1>${await photo(e.photo, "large")}<div class="prose">${await blocks(e.body)}</div>`;
}

function price(value) {
  return typeof value === "number" ? `€${value.toFixed(0)}` : "";
}

async function menu() {
  const data = await api("/api/v2/menus/main/");
  document.getElementById("menu").innerHTML = data.items
    .map((item) => (item.is_page ? link(item.url, item.label) : `<a href="${CMS}${item.url}">${escape(item.label)}</a>`))
    .join("");
}

async function main() {
  const box = document.getElementById("page");
  try {
    const [page] = await Promise.all([loadPage(), menu()]);
    const draw = templates[page.meta.type] || fallback;
    box.innerHTML = await draw(page);
    document.title = `${page.seo_title || page.title} — Clay & Kiln`;
    document.getElementById("preview-bar").hidden = !previewToken;
  } catch (err) {
    box.innerHTML = `<h1>Page not found</h1><p>${escape(err.message)}</p><p>${link("/", "Go to the home page")}</p>`;
  }
}

main();
