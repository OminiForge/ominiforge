// Injected into every generated page. Fetches the site-wide versions.json and
// renders a version dropdown in the mdbook menu bar, letting readers jump to
// the same page in another release (falling back to that release's index when
// the page doesn't exist there).
(function () {
  // Site root = current version dir's parent. Pages live at <root>/<ver>/<path>.
  var path = window.location.pathname;
  // Find the version segment: the directory right after the site root. Since we
  // don't know the deploy base, derive it from the <html data-version> we add,
  // or default to going up from the current page to its version root.
  function siteRoot() {
    // versions.json sits at the site root; the current version is one level up
    // from the deepest mdbook asset. Simplest robust approach: walk up until
    // versions.json resolves.
    var seg = path.split("/").filter(Boolean);
    // Try progressively shorter prefixes.
    return seg.slice(0, -1).join("/");
  }

  function fetchVersions() {
    var candidates = ["../versions.json", "../../versions.json", "/versions.json"];
    var chain = Promise.reject();
    candidates.forEach(function (u) {
      chain = chain.catch(function () {
        return fetch(u).then(function (r) {
          if (!r.ok) throw new Error(u);
          return { url: u, data: r.json() };
        });
      });
    });
    return chain.then(function (x) {
      return x.data.then(function (d) {
        return { base: x.url.replace(/versions\.json$/, ""), data: d };
      });
    });
  }

  function currentSubPath() {
    // The page path relative to its version root, e.g. "design/architecture.html".
    var parts = path.split("/").filter(Boolean);
    return parts.length > 1 ? parts.slice(1).join("/") : "index.html";
  }

  fetchVersions().then(function (res) {
    var data = res.data, base = res.base;
    var bar = document.querySelector(".menu-bar") || document.body;
    var wrap = document.createElement("div");
    wrap.id = "version-switcher";
    var sel = document.createElement("select");
    sel.setAttribute("aria-label", "Documentation version");
    var sub = currentSubPath();
    data.versions.forEach(function (v) {
      var opt = document.createElement("option");
      opt.value = v.path;
      opt.textContent = v.name + (v.path === data.latest ? " (latest)" : "");
      sel.appendChild(opt);
    });
    // Best-effort current selection.
    var here = path.split("/").filter(Boolean);
    if (here.length) sel.value = here[0];
    sel.addEventListener("change", function () {
      window.location.href = base + sel.value + "/" + sub;
    });
    var label = document.createElement("span");
    label.textContent = "Version: ";
    wrap.appendChild(label);
    wrap.appendChild(sel);
    bar.appendChild(wrap);
  }).catch(function () {
    /* versions.json not found (e.g. local `just doc`) — switcher stays hidden. */
  });
})();
