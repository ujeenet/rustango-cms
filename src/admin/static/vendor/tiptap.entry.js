// Vendored TipTap richtext engine for rustango-cms (#294).
//
// Rebuilt to add move-safe internal-link support. Config matches the
// prior bundle (StarterKit headings h2-h4 + Link, openOnClick off) so
// existing editor behaviour is unchanged — only the Link mark gains
// linktype/id awareness.
import { Editor } from "@tiptap/core";
import StarterKit from "@tiptap/starter-kit";
import Link from "@tiptap/extension-link";
import Image from "@tiptap/extension-image";
import Table from "@tiptap/extension-table";
import TableRow from "@tiptap/extension-table-row";
import TableHeader from "@tiptap/extension-table-header";
import TableCell from "@tiptap/extension-table-cell";

// Wagtail-style move-safe links store the target as `linktype` + `id`
// (and carry NO href) so they survive page/media moves; the server-side
// `| richtext` filter resolves them to real URLs at render time. Stock
// TipTap Link only understands `href`, so without this it (a) fails to
// recognise href-less internal anchors at all, and (b) drops the
// linktype/id attributes on edit — silently breaking move-safe links.
const RcmsLink = Link.extend({
  addAttributes() {
    return {
      ...this.parent?.(),
      linktype: {
        default: null,
        parseHTML: (el) => el.getAttribute("linktype"),
        renderHTML: (attrs) => (attrs.linktype ? { linktype: attrs.linktype } : {}),
      },
      id: {
        default: null,
        parseHTML: (el) => el.getAttribute("id"),
        renderHTML: (attrs) =>
          attrs.id !== null && attrs.id !== undefined && attrs.id !== ""
            ? { id: String(attrs.id) }
            : {},
      },
    };
  },
  parseHTML() {
    // Match external anchors (href) AND move-safe internal anchors
    // (linktype — these have no href, so the stock `a[href]` rule misses
    // them).
    return [
      { tag: 'a[href]:not([href *= "javascript:" i])' },
      { tag: "a[linktype]" },
    ];
  },
});

window.RcmsRichtext = {
  create({ element, content, onUpdate }) {
    return new Editor({
      element,
      content: content || "",
      extensions: [
        StarterKit.configure({ heading: { levels: [2, 3, 4] } }),
        // Match the prior bundle's link rendering: rel="noopener", no
        // target (internal links shouldn't open in a new tab).
        RcmsLink.configure({
          openOnClick: false,
          HTMLAttributes: { rel: "noopener", target: null },
        }),
        // Images + tables. Beyond enabling insertion, registering these
        // nodes means existing `<img>`/`<table>` markup in a body is
        // preserved on edit instead of being silently stripped (no node
        // = no parse rule). Table is non-resizable so cells stay plain
        // <td>/<th> (no `colwidth` the sanitizer would drop).
        Image,
        Table,
        TableRow,
        TableHeader,
        TableCell,
      ],
      onUpdate: ({ editor }) => {
        if (onUpdate) onUpdate(editor.getHTML());
      },
    });
  },
};
