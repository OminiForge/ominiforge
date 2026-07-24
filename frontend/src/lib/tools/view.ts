/** Parsing of the backend's UI diff view (`Content::TextView`, `doc/tool-view.md`)
 *  into per-file hunks for rendering. The view text is one or more unified-diff
 *  blocks, each introduced by `--- a/PATH` / `+++ b/PATH` headers; a `write`
 *  new-file view is raw content (no headers) and is handled by the caller via
 *  `CodeView` instead of this splitter. Pure presentation parsing — no diff
 *  construction, no file state. */

export interface ViewFile {
	path: string;
	/** The hunk body (`@@`/` `/`-`/`+` lines) for `Diff.svelte`. */
	diff: string;
}

/** Split a view text into its per-file diff blocks. Header lines are consumed;
 *  the path comes from the `+++ b/PATH` line (the "after" name). A view with
 *  no recognizable header yields no files (callers fall back to raw content). */
export function splitViewFiles(view: string): ViewFile[] {
	const files: ViewFile[] = [];
	let path: string | undefined;
	let body: string[] = [];
	const flush = () => {
		if (path !== undefined) files.push({ path, diff: body.join('\n').trimEnd() });
	};
	for (const line of view.split('\n')) {
		if (line.startsWith('--- a/')) {
			flush();
			path = undefined;
			body = [];
		} else if (line.startsWith('+++ b/')) {
			path = line.slice('+++ b/'.length);
		} else if (path !== undefined) {
			body.push(line);
		}
	}
	flush();
	return files;
}
