// Injected into every generated page. Fetches the site-wide versions.json and
// renders a version dropdown in the mdbook menu bar, letting readers jump to
// the same page in another release (falling back to that release's index when
// the page doesn't exist there).
//
// Path model: the site is deployed under an unknown base path (e.g.
// https://user.github.io/<repo>/). Every page lives at
//   <base>/<version>/<page...>
// versions.json lives at <base>/versions.json. We recover <base> and
// <version> by locating the known version directory name in the URL, so the
// switcher works regardless of how deep the deploy base is.
(function () {
  var KNOWN_VERSIONS = ["dev"]; // release tags (vX.Y.Z) are matched by regex
  var VERSION_RE = /^v\d+\.\d+\.\d+$/;

  function isVersion(seg) {
    return VERSION_RE.test(seg) || KNOWN_VERSIONS.indexOf(seg) !== -1;
  }

  // Split the current URL into { base, version, subPath }.
  // Returns null when no version segment is found (shouldn't happen on the
  // deployed site).
  function locate() {
    var segs = window.location.pathname.split("/").filter(Boolean);
    for (var i = segs.length - 1; i >= 0; i--) {
      if (isVersion(segs[i])) {
        return {
          base: "/" + segs.slice(0, i).join("/") + (i > 0 ? "/" : "/"),
          version: segs[i],
          subPath: segs.slice(i + 1).join("/") || "index.html",
        };
      }
    }
    return null;
  }

  var here = locate();
  if (!here) return; // not on a versioned page — stay hidden.

  // Normalise base to always end with exactly one '/'.
  var base = here.base.replace(/\/*$/, "/");

  fetch(base + "versions.json")
    .then(function (r) {
      if (!r.ok) throw new Error("versions.json " + r.status);
      return r.json();
    })
    .then(function (data) {
      var bar = document.querySelector(".menu-bar") || document.body;
      var wrap = document.createElement("div");
      wrap.id = "version-switcher";
      var sel = document.createElement("select");
      sel.setAttribute("aria-label", "Documentation version");
      data.versions.forEach(function (v) {
        var opt = document.createElement("option");
        opt.value = v.path;
        opt.textContent = v.name + (v.path === data.latest ? " (latest)" : "");
        sel.appendChild(opt);
      });
      sel.value = here.version;
      sel.addEventListener("change", function () {
        // Try the same page in the chosen version; if it doesn't exist there
        // (404), fall back to that version's index.
        var target = base + sel.value + "/" + here.subPath;
        fetch(target, { method: "HEAD" })
          .then(function (r) {
            window.location.href = r.ok
              ? target
              : base + sel.value + "/index.html";
          })
          .catch(function () {
            window.location.href = base + sel.value + "/index.html";
          });
      });
      var label = document.createElement("span");
      label.textContent = "Version: ";
      wrap.appendChild(label);
      wrap.appendChild(sel);
      bar.appendChild(wrap);
    })
    .catch(function () {
      /* versions.json not found (e.g. local `just doc`) — switcher stays hidden. */
    });
})();
