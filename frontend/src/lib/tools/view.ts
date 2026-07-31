/** Structured UI view from the backend (`Content::TextView`, `doc/tool-view.md`).
 *  The `text` field is a JSON envelope `{ kind, ... }` where `kind` is one of
 *  the closed variants below. The front-end dispatches on `kind` to the
 *  matching renderer — it never parses model-facing text formats. */

/** One file's diff hunk (unified-diff text with `--- a/` / `+++ b/` headers). */
export interface DiffFile {
	path: string;
	patch: string;
}

/** A file's content. Whole-file reads carry raw content (numbered 1..N by
 *  the renderer); a ranged read carries the WHOLE file plus the resolved
 *  window (`numbered: true` + `start`/`end`) — the renderer slices to the
 *  window, gutter numbered by the window's absolute lines. */
export interface CodeView {
	kind: 'code';
	path: string;
	content: string;
	/** True when `content` rows are already numbered (`N:line`, absolute). */
	numbered?: boolean;
	/** The requested (pre-clamp) 1-based inclusive window, for ranged reads. */
	start?: number;
	end?: number;
}

/** A terminal command + output + exit code. */
export interface TerminalView {
	kind: 'terminal';
	command: string;
	output: string;
	exit_code?: number;
}

/** A directory listing. */
export interface ListingView {
	kind: 'listing';
	path: string;
	entries: string[];
}

/** A markdown document. */
export interface MarkdownView {
	kind: 'markdown';
	text: string;
}

/** Plain text (MCP tools, future built-ins without a dedicated view). */
export interface PlainView {
	kind: 'plain';
	text: string;
}

/** A diff across one or more files. */
export interface DiffView {
	kind: 'diff';
	files: DiffFile[];
}

/** The closed set of structured UI views. */
export type ToolView =
	| DiffView
	| CodeView
	| TerminalView
	| ListingView
	| MarkdownView
	| PlainView;

/** Parse a `Content::TextView` JSON envelope into a structured `ToolView`.
 *  Returns `null` when the text isn't valid JSON or lacks a recognized `kind`
 *  (legacy logs, MCP tools without a view) — the caller falls back to
 *  `GenericResult` (raw args + result). */
export function parseView(text: string): ToolView | null {
	let parsed: unknown;
	try {
		parsed = JSON.parse(text);
	} catch {
		return null;
	}
	if (!parsed || typeof parsed !== 'object') return null;
	const obj = parsed as Record<string, unknown>;
	const kind = obj.kind;
	if (typeof kind !== 'string') return null;

	switch (kind) {
		case 'diff': {
			const files = obj.files;
			if (!Array.isArray(files)) return null;
			return {
				kind: 'diff',
				files: files
					.filter((f): f is Record<string, unknown> => !!f && typeof f === 'object')
					.map((f) => ({
						path: String(f.path ?? ''),
						patch: String(f.patch ?? '')
					}))
			};
		}
		case 'code':
			return {
				kind: 'code',
				path: String(obj.path ?? ''),
				content: String(obj.content ?? ''),
				numbered: obj.numbered === true ? true : undefined,
				start: typeof obj.start === 'number' ? obj.start : undefined,
				end: typeof obj.end === 'number' ? obj.end : undefined
			};
		case 'terminal':
			return {
				kind: 'terminal',
				command: String(obj.command ?? ''),
				output: String(obj.output ?? ''),
				exit_code: typeof obj.exit_code === 'number' ? obj.exit_code : undefined
			};
		case 'listing':
			return {
				kind: 'listing',
				path: String(obj.path ?? ''),
				entries: Array.isArray(obj.entries) ? obj.entries.map(String) : []
			};
		case 'markdown':
			return { kind: 'markdown', text: String(obj.text ?? '') };
		case 'plain':
			return { kind: 'plain', text: String(obj.text ?? '') };
		default:
			return null;
	}
}
