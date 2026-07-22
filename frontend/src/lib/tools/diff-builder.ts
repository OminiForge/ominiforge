/** Client-side diff construction for the `edit`/`write` tool cards.
 *
 *  The backend no longer echoes a diff in the tool result (see
 *  `doc/tool-protocol.md` §11.4) — the model already wrote `old`/`new` in its
 *  own call arguments, so the diff is redundant there. The frontend rebuilds a
 *  contextual unified diff from those arguments plus a cache of the file's
 *  content (populated in `conversation.ts` from committed read/write events —
 *  `edit` deliberately does not feed it, since the preview needs the pre-edit
 *  content). The hunk-merge/context algorithm is self-contained here (it was
 *  originally ported from the old backend's diff renderer, since deleted),
 *  and building it client-side is what lets the diff render incrementally as
 *  `Delta::ToolArgs` streams the arguments in.
 *
 *  `write`'s overwrite case has no anchor correspondence between old and new
 *  (they're two independent whole-file blobs, unlike `edit`'s already-anchored
 *  `old`/`new`), so `buildWriteDiff` runs a real line-level diff (`diffArrays`
 *  from `diff`/jsdiff) instead of a mechanical splice.
 *
 *  Everything here is pure — no Svelte, no I/O — so it is unit-tested directly
 *  against the Rust-side fixtures. */

import { diffArrays } from 'diff';

/** One content-anchored replacement, mirroring `EditEntryArg` in `edit.rs`. */
export interface EditEntry {
	path: string;
	old: string[];
	new: string[];
	replace_all?: boolean;
}

/** A file's diff plus any preview caveat the UI should surface separately (so
 *  it never gets mistaken for diff content). `note` is set when the preview is
 *  a best-effort degradation, not an exact render of what the backend did. */
export interface FileDiff {
	diff: string;
	note?: string;
}

/** Unchanged context lines shown on each side of a hunk. */
const CONTEXT = 3;

/** Every start index (0-based) at which `needle` occurs as a contiguous run in
 *  `haystack`, scanning left to right and skipping past a match so overlapping
 *  runs are not double-counted. Mirrors `find_matches` in `edit.rs`. */
function findMatches(haystack: string[], needle: string[]): number[] {
	if (needle.length === 0 || needle.length > haystack.length) return [];
	const starts: number[] = [];
	let i = 0;
	while (i + needle.length <= haystack.length) {
		let hit = true;
		for (let j = 0; j < needle.length; j++) {
			if (haystack[i + j] !== needle[j]) {
				hit = false;
				break;
			}
		}
		if (hit) {
			starts.push(i);
			i += needle.length;
		} else {
			i += 1;
		}
	}
	return starts;
}

/** A resolved edit against the file: a half-open `[start, end)` old range and
 *  the replacement payload. */
interface Splice {
	start: number;
	end: number;
	payload: string[];
}

/** Build the unified-diff text for one file, given its cached lines (or
 *  `undefined` on a cache miss) and every edit entry targeting it.
 *
 *  Preview degradations (never a hard failure — the authoritative outcome is
 *  the backend's terse result, reflected in the card status):
 *  - cache miss → no context available; a bare `-old`/`+new` block per entry.
 *  - an `old` that matches nowhere in the cache → same bare block for that
 *    entry (the cache is stale relative to disk).
 *  - an `old` that matches more than once without `replace_all` → the first
 *    match is previewed; `note` flags it. */
export function buildFileDiff(
	cacheLines: string[] | undefined,
	edits: EditEntry[],
	context = CONTEXT
): FileDiff {
	if (cacheLines === undefined) {
		return { diff: edits.map(bareBlock).join('\n'), note: '无上下文（该文件未在本会话读取）' };
	}

	const splices: Splice[] = [];
	const bareParts: string[] = [];
	let ambiguous = false;
	let stale = false;

	for (const e of edits) {
		const matches = findMatches(cacheLines, e.old);
		if (matches.length === 0) {
			// Cache is stale relative to what the model quoted; can't place a
			// contextual hunk, so fall back to a bare block for this entry.
			stale = true;
			bareParts.push(bareBlock(e));
			continue;
		}
		if (matches.length > 1 && !e.replace_all) {
			ambiguous = true;
			const start = matches[0];
			splices.push({ start, end: start + e.old.length, payload: e.new });
			continue;
		}
		for (const start of matches) {
			splices.push({ start, end: start + e.old.length, payload: e.new });
		}
	}

	// Render contextual hunks for the placeable splices, then append any bare
	// blocks for entries that couldn't be placed.
	const parts: string[] = [];
	const hunks = renderHunks(cacheLines, splices, context);
	if (hunks) parts.push(hunks);
	if (bareParts.length) parts.push(bareParts.join('\n'));

	const notes: string[] = [];
	if (ambiguous) notes.push('预览取首个匹配，以执行结果为准');
	if (stale) notes.push('部分内容未在当前缓存中定位（可能已过时）');
	const diff: FileDiff = { diff: parts.join('\n') };
	if (notes.length) diff.note = notes.join('；');
	return diff;
}

/** A context-less `-old`/`+new` block for an entry that can't be located. */
function bareBlock(e: EditEntry): string {
	const lines = [...e.old.map((l) => `-${l}`), ...e.new.map((l) => `+${l}`)];
	return lines.join('\n');
}

/** Render sorted splices into unified-diff hunks with `context` lines on each
 *  side, merging hunks whose context windows touch. Self-contained (originally
 *  ported from the old backend's diff renderer, since deleted); `lines` is the
 *  file's pre-edit content. */
function renderHunks(lines: string[], splicesIn: Splice[], context: number): string {
	if (splicesIn.length === 0) return '';
	const n = lines.length;
	const splices = [...splicesIn].sort((a, b) => a.start - b.start);

	// Group edits whose context windows overlap into shared hunks.
	const groups: Splice[][] = [];
	for (const s of splices) {
		const last = groups[groups.length - 1];
		const prevEnd = last ? last[last.length - 1].end : undefined;
		if (prevEnd !== undefined && s.start <= prevEnd + context * 2) {
			last.push(s);
		} else {
			groups.push([s]);
		}
	}

	const out: string[] = [];
	let cumAdded = 0;
	let cumRemoved = 0;
	for (const group of groups) {
		const firstStart = group[0].start;
		const lastEnd = group[group.length - 1].end;
		const ctxStart = Math.max(0, firstStart - context);
		const ctxEnd = Math.min(lastEnd + context, n);

		const oldLen = ctxEnd - ctxStart;
		let newLen = oldLen;
		for (const s of group) newLen = newLen - (s.end - s.start) + s.payload.length;
		const oldStart = ctxStart + 1;
		const newStart = Math.max(0, ctxStart + cumAdded - cumRemoved) + 1;
		out.push(`@@ -${oldStart},${oldLen} +${newStart},${newLen} @@`);

		let cursor = ctxStart;
		for (const s of group) {
			for (let i = cursor; i < s.start; i++) out.push(` ${lines[i]}`);
			for (let i = s.start; i < s.end; i++) out.push(`-${lines[i]}`);
			for (const p of s.payload) out.push(`+${p}`);
			cursor = s.end;
			cumAdded += s.payload.length;
			cumRemoved += s.end - s.start;
		}
		for (let i = cursor; i < ctxEnd; i++) out.push(` ${lines[i]}`);
	}
	return out.join('\n');
}

/** Build a unified-diff string for a `write` overwrite, given the pre-write
 *  cached content (`oldLines`) and the new content (`newLines`) as line arrays.
 *
 *  Unlike `buildFileDiff` (which uses mechanical splice placement for already-
 *  anchored `edit` `old`/`new` pairs), this runs a real Myers/LCS line diff via
 *  `diffArrays` from the `diff` package — necessary because a `write` args has
 *  only the new full content, and the old comes from the file cache; there is no
 *  anchor correspondence between them.
 *
 *  Returns a `FileDiff` with the same `@@`-hunk text shape that `Diff.svelte`
 *  parses, or `diff: ''` when the content is identical. */
export function buildWriteDiff(
	oldLines: string[],
	newLines: string[],
	context = CONTEXT
): FileDiff {
	const changes = diffArrays(oldLines, newLines);

	// Flatten into a per-line list that carries both-side positions.
	interface LineChange {
		tag: 'eq' | 'add' | 'del';
		text: string;
		oldIdx: number; // 0-based index in oldLines; -1 for add-only lines
		newIdx: number; // 0-based index in newLines; -1 for del-only lines
	}

	const flat: LineChange[] = [];
	let oi = 0,
		ni = 0;
	for (const ch of changes) {
		if (ch.removed) {
			for (const l of ch.value) flat.push({ tag: 'del', text: l, oldIdx: oi++, newIdx: -1 });
		} else if (ch.added) {
			for (const l of ch.value) flat.push({ tag: 'add', text: l, oldIdx: -1, newIdx: ni++ });
		} else {
			for (const l of ch.value) flat.push({ tag: 'eq', text: l, oldIdx: oi++, newIdx: ni++ });
		}
	}

	// Indices (into flat[]) of lines that actually changed.
	const changedIdx = flat.map((_, i) => i).filter((i) => flat[i].tag !== 'eq');
	if (changedIdx.length === 0) return { diff: '' };

	// Expand each changed index into a context window and merge overlapping windows.
	const windows: [number, number][] = [];
	for (const ci of changedIdx) {
		const lo = Math.max(0, ci - context);
		const hi = Math.min(flat.length, ci + context + 1);
		const last = windows[windows.length - 1];
		if (last && last[1] >= lo) {
			last[1] = Math.max(last[1], hi);
		} else {
			windows.push([lo, hi]);
		}
	}

	// Render each window as a unified-diff hunk.
	const out: string[] = [];
	for (const [lo, hi] of windows) {
		const hunk = flat.slice(lo, hi);
		// First old-side line in this hunk (0-based), or 0 if purely added.
		const firstOld = hunk.find((l) => l.oldIdx !== -1);
		const firstNew = hunk.find((l) => l.newIdx !== -1);
		const oldStart = firstOld ? firstOld.oldIdx + 1 : 0;
		const newStart = firstNew ? firstNew.newIdx + 1 : 0;
		const oldLen = hunk.filter((l) => l.tag !== 'add').length;
		const newLen = hunk.filter((l) => l.tag !== 'del').length;
		out.push(`@@ -${oldStart},${oldLen} +${newStart},${newLen} @@`);
		for (const l of hunk) {
			const prefix = l.tag === 'add' ? '+' : l.tag === 'del' ? '-' : ' ';
			out.push(`${prefix}${l.text}`);
		}
	}

	return { diff: out.join('\n') };
}

/** Extract the fully-closed entries of an `edits` array from a possibly
 *  incomplete args JSON string (as it accumulates over `Delta::ToolArgs`).
 *
 *  Scans with brace/bracket + string-state tracking rather than `JSON.parse`
 *  (which would throw on the truncated tail), slicing out each top-level
 *  `{...}` element of the `edits` array and parsing it on its own. The final,
 *  still-open element is skipped until a later delta closes it. Returns `[]`
 *  before the array has even started. Pure and idempotent — call fresh on
 *  every args update, no parser state to carry. */
export function parsePartialEdits(argsSoFar: string): EditEntry[] {
	const key = argsSoFar.indexOf('"edits"');
	if (key === -1) return [];
	// The array opens at the first `[` after the key.
	let i = argsSoFar.indexOf('[', key);
	if (i === -1) return [];
	i += 1;

	const entries: EditEntry[] = [];
	let depth = 0; // object-brace depth within the array
	let start = -1; // start index of the current top-level object
	let inString = false;
	let escaped = false;

	for (; i < argsSoFar.length; i++) {
		const c = argsSoFar[i];
		if (inString) {
			if (escaped) escaped = false;
			else if (c === '\\') escaped = true;
			else if (c === '"') inString = false;
			continue;
		}
		if (c === '"') {
			inString = true;
		} else if (c === '{') {
			if (depth === 0) start = i;
			depth += 1;
		} else if (c === '}') {
			depth -= 1;
			if (depth === 0 && start !== -1) {
				const slice = argsSoFar.slice(start, i + 1);
				const entry = tryParseEntry(slice);
				if (entry) entries.push(entry);
				start = -1;
			}
		} else if (c === ']' && depth === 0) {
			// End of the edits array.
			break;
		}
	}
	return entries;
}

/** Parse one already-sliced object into an `EditEntry`, tolerating anything
 *  that isn't the expected shape (returns `null` so a malformed element is
 *  skipped rather than throwing mid-stream). */
function tryParseEntry(slice: string): EditEntry | null {
	let obj: unknown;
	try {
		obj = JSON.parse(slice);
	} catch {
		return null;
	}
	if (!obj || typeof obj !== 'object') return null;
	const o = obj as Record<string, unknown>;
	if (typeof o.path !== 'string') return null;
	if (!Array.isArray(o.old) || !Array.isArray(o.new)) return null;
	if (!o.old.every((l) => typeof l === 'string') || !o.new.every((l) => typeof l === 'string')) {
		return null;
	}
	const entry: EditEntry = { path: o.path, old: o.old as string[], new: o.new as string[] };
	if (typeof o.replace_all === 'boolean') entry.replace_all = o.replace_all;
	return entry;
}
