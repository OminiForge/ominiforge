#!/usr/bin/env bash
# Build the mdbook documentation for every released tag plus the current
# development (master) docs, into a single static site:
#
#   site/
#     index.html          -> redirects to the latest stable version
#     dev/                -> docs built from master (unreleased)
#     v0.1.0/             -> docs built from the v0.1.0 tag
#     ...
#     versions.json       -> consumed by the version switcher
#
# mdbook has no built-in versioning; this follows the rustdoc/docs.rs pattern of
# building each ref into its own subdirectory and letting a small JS switcher
# jump between them.
#
# Usage:  doc/build-all-versions.sh [output-dir]   (default: doc/site)
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
OUT="${1:-$ROOT/doc/site}"
SITE="$OUT"
rm -rf "$SITE"
mkdir -p "$SITE"

# Released versions: all semver tags (vX.Y.Z), newest first.
mapfile -t TAGS < <(git tag --list 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname)

build_one() {
  local ref="$1" dest="$2"
  echo "== building docs for $ref -> $dest"
  # Materialise the ref's doc/ tree without touching the working copy.
  local tmp
  tmp="$(mktemp -d)"
  git --work-tree="$tmp" checkout --force --quiet "$ref" -- doc 2>/dev/null || {
    echo "   (no doc/ at $ref, skipping)"; rm -rf "$tmp"; return 0;
  }
  if [ -f "$tmp/doc/book.toml" ]; then
    mdbook build "$tmp/doc" --dest-dir "$dest" >/dev/null
  else
    mkdir -p "$dest"
    echo "<p>No documentation at $ref.</p>" > "$dest/index.html"
  fi
  rm -rf "$tmp"
}

# dev docs from the current master checkout.
build_one "master" "$SITE/dev"

# released versions.
for tag in "${TAGS[@]}"; do
  build_one "$tag" "$SITE/$tag"
done

# versions.json consumed by the switcher.
LATEST="${TAGS[0]:-dev}"
{
  printf '{\n  "latest": "%s",\n  "versions": [\n' "$LATEST"
  first=1
  for tag in "${TAGS[@]}"; do
    [ $first -eq 0 ] && printf ',\n'
    printf '    {"name": "%s", "path": "%s"}' "$tag" "$tag"
    first=0
  done
  [ $first -eq 0 ] && printf ',\n'
  printf '    {"name": "dev (master)", "path": "dev"}\n  ]\n}\n'
} > "$SITE/versions.json"

# root redirect to the latest stable (or dev when nothing released yet).
cat > "$SITE/index.html" <<HTML
<!doctype html>
<meta http-equiv="refresh" content="0; url=./$LATEST/">
<title>Ominiforge docs</title>
<a href="./$LATEST/">Ominiforge documentation ($LATEST)</a>
HTML

echo "== done. latest=$LATEST, site at $SITE"
