import fs from "node:fs";
const R = "node_modules/codemirror";
const JS_PARTS = [
  "lib/codemirror.js",
  "addon/mode/overlay.js",
  "mode/xml/xml.js",
  "mode/javascript/javascript.js",
  "mode/css/css.js",
  "mode/htmlmixed/htmlmixed.js",
  "addon/edit/matchbrackets.js",
  "addon/edit/closetag.js",
  "addon/selection/active-line.js",
  "addon/display/placeholder.js",
  "addon/dialog/dialog.js",
  "addon/search/searchcursor.js",
  "addon/search/search.js",
  "addon/comment/comment.js",
];
const CSS_PARTS = ["lib/codemirror.css", "addon/dialog/dialog.css"];

const js = JS_PARTS.map((p) => fs.readFileSync(`${R}/${p}`, "utf8")).join("\n;\n");
const css = CSS_PARTS.map((p) => fs.readFileSync(`${R}/${p}`, "utf8")).join("\n");

// A Tera mode: htmlmixed with a delimiter overlay. Tera is Jinja2-shaped,
// so `{{ }}`, `{% %}` and `{# #}` are what need to stand out against HTML.
const tera = `
CodeMirror.defineMode("tera", function (config) {
  var overlay = {
    startState: function () { return { inTag: null }; },
    token: function (stream, state) {
      if (state.inTag) {
        var close = state.inTag === "{#" ? "#}" : (state.inTag === "{{" ? "}}" : "%}");
        while (!stream.eol()) {
          if (stream.match(close)) { var t = state.inTag; state.inTag = null;
            return t === "{#" ? "comment" : (t === "{{" ? "variable-2" : "keyword"); }
          stream.next();
        }
        return state.inTag === "{#" ? "comment" : (state.inTag === "{{" ? "variable-2" : "keyword");
      }
      if (stream.match("{#")) { state.inTag = "{#"; return "comment"; }
      if (stream.match("{{")) { state.inTag = "{{"; return "variable-2"; }
      if (stream.match("{%")) { state.inTag = "{%"; return "keyword"; }
      while (stream.next() != null && !stream.match("{{", false)
             && !stream.match("{%", false) && !stream.match("{#", false)) {}
      return null;
    }
  };
  return CodeMirror.overlayMode(CodeMirror.getMode(config, "htmlmixed"), overlay);
});
CodeMirror.defineMIME("text/x-tera", "tera");
`;

// Self-injecting CSS keeps this genuinely one file, matching how
// tiptap.bundle.js is shipped.
const inject = `
(function () {
  if (document.getElementById("rcms-cm-css")) return;
  var s = document.createElement("style");
  s.id = "rcms-cm-css";
  s.textContent = ${JSON.stringify(css)};
  document.head.appendChild(s);
})();
`;

const out = `/* CodeMirror 5 (MIT) — vendored bundle. See codemirror.bundle.README.md.
   Do not hand-edit: regenerate with the recipe in that README. */
${js}
;${tera}
;${inject}
`;
fs.writeFileSync("codemirror.bundle.js", out);
console.log("  bundle:", (out.length / 1024).toFixed(0), "KB raw");
