/* rustango-cms default analytics beacon. Vanilla, no build, no deps.
   Pageview  -> HTTP POST /__cms__/collect (CSRF-exempt, no-store).
   Engagement + "active now" -> WebSocket /__cms__/ws (server times the
   connection). If the socket never opens, an unload POST beacon is the
   fallback. Analytics must never throw into the page. */
(function () {
  try {
    if (navigator.webdriver) return;
    if (navigator.globalPrivacyControl === true) return;
    var dnt = navigator.doNotTrack || window.doNotTrack || navigator.msDoNotTrack;
    if (dnt === "1" || dnt === "yes") return;

    var COLLECT = "/__cms__/collect";
    var WS_PATH = "/__cms__/ws";

    function uuid() {
      try {
        if (crypto && crypto.randomUUID) return crypto.randomUUID();
      } catch (e) {}
      return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, function (c) {
        var r = (Math.random() * 16) | 0;
        return (c === "x" ? r : (r & 0x3) | 0x8).toString(16);
      });
    }
    function stored(store, key) {
      try {
        var v = store.getItem(key);
        if (!v) { v = uuid(); store.setItem(key, v); }
        return v;
      } catch (e) { return uuid(); } // private mode -> ephemeral
    }

    var vid = stored(localStorage, "rcms_vid");
    var sid = stored(sessionStorage, "rcms_sid");
    var path = location.pathname;
    var referrer = document.referrer || "";
    var loc = document.documentElement.lang || "";

    function post(body) {
      body.vid = vid; body.sid = sid; body.p = path;
      var json = JSON.stringify(body);
      try {
        var blob = new Blob([json], { type: "application/json" });
        if (navigator.sendBeacon && navigator.sendBeacon(COLLECT, blob)) return;
      } catch (e) {}
      try {
        fetch(COLLECT, { method: "POST", body: json, keepalive: true,
          headers: { "Content-Type": "application/json" } });
      } catch (e) {}
    }

    /* 1) Pageview — always over HTTP so it survives even if WS is blocked. */
    post({ t: "pageview", referrer: referrer, loc: loc,
           sw: (screen.width | 0), sh: (screen.height | 0) });

    /* 2) Engagement + presence — WebSocket. The server records the
          engagement row (with duration) when this socket closes. */
    var maxScroll = 0, start = Date.now(), wsOpen = false, ended = false;
    function scrollPct() {
      var el = document.documentElement, b = document.body;
      var full = Math.max(el.scrollHeight, b ? b.scrollHeight : 0) - el.clientHeight;
      if (full <= 0) return 100;
      var top = window.pageYOffset || el.scrollTop || 0;
      var p = Math.round((top / full) * 100);
      return p < 0 ? 0 : (p > 100 ? 100 : p);
    }
    addEventListener("scroll", function () {
      var p = scrollPct();
      if (p > maxScroll) maxScroll = p;
    }, { passive: true });

    var ws = null;
    try {
      var proto = location.protocol === "https:" ? "wss:" : "ws:";
      var q = "?vid=" + encodeURIComponent(vid) + "&sid=" + encodeURIComponent(sid) +
              "&p=" + encodeURIComponent(path) + "&loc=" + encodeURIComponent(loc) +
              "&referrer=" + encodeURIComponent(referrer);
      ws = new WebSocket(proto + "//" + location.host + WS_PATH + q);
      ws.onopen = function () { wsOpen = true; };
      // Push scroll depth periodically so the server has it at close.
      setInterval(function () {
        try { if (ws && ws.readyState === 1) ws.send(JSON.stringify({ scroll: maxScroll })); }
        catch (e) {}
      }, 10000);
    } catch (e) { ws = null; }

    /* 3) Fallback — if the socket never opened, report engagement over
          HTTP on unload so we don't lose it entirely. */
    function flush() {
      if (ended) return; ended = true;
      if (wsOpen) { try { ws.close(); } catch (e) {} return; } // server handles it
      post({ t: "engagement", loc: loc,
             dur: Date.now() - start, scroll: maxScroll });
    }
    addEventListener("pagehide", flush);
    addEventListener("visibilitychange", function () {
      if (document.visibilityState === "hidden") flush();
    });
  } catch (e) { /* never break the page */ }
})();
