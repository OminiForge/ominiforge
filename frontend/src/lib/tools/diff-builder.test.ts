import { describe, expect, it } from 'vitest';
import { buildFileDiff, buildWriteDiff, parsePartialEdits } from './diff-builder';

describe('buildFileDiff', () => {
	// The expected strings were originally cross-checked against the old
	// backend's unified-diff test fixtures (since deleted — this renderer is
	// now self-contained): same hunk-header/context-merge structure, adapted
	// from line-number anchors to content anchors (unique per-line text instead
	// of `"x"` repeated, since content matching needs distinct lines to avoid
	// ambiguity where the old line-number scheme didn't care).

	it('renders one hunk with context for a single-line replace', () => {
		const cache = ['a', 'b', 'c', 'd', 'e'];
		const out = buildFileDiff(cache, [{ path: 'f', old: ['c'], new: ['C'] }], 1);
		expect(out.diff).toBe('@@ -2,3 +2,3 @@\n b\n-c\n+C\n d');
		expect(out.note).toBeUndefined();
	});

	it('splits distant edits into two hunks', () => {
		const cache = Array.from({ length: 12 }, (_, i) => `l${i + 1}`);
		const out = buildFileDiff(
			cache,
			[
				{ path: 'f', old: ['l2'], new: ['A'] },
				{ path: 'f', old: ['l11'], new: ['B'] }
			],
			1
		);
		expect(out.diff).toBe(
			'@@ -1,3 +1,3 @@\n l1\n-l2\n+A\n l3\n@@ -10,3 +10,3 @@\n l10\n-l11\n+B\n l12'
		);
	});

	it('merges close edits into one hunk', () => {
		const cache = ['a', 'b', 'c', 'd', 'e'];
		const out = buildFileDiff(
			cache,
			[
				{ path: 'f', old: ['b'], new: ['B'] },
				{ path: 'f', old: ['d'], new: ['D'] }
			],
			3
		);
		expect(out.diff).toBe('@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n-d\n+D\n e');
	});

	it('an insert keeps the anchor line in both old and new (the anchor renders as context, not a remove+add pair)', () => {
		const cache = ['a', 'b'];
		const out = buildFileDiff(cache, [{ path: 'f', old: ['a'], new: ['a', 'A1'] }], 1);
		// The shared prefix `a` is stripped from the splice core, so it shows as
		// a context line and only the truly-inserted line is `+`.
		expect(out.diff).toBe('@@ -1,2 +1,3 @@\n a\n+A1\n b');
	});

	// A model that quotes MORE than it actually changed (a common behavior —
	// it pads `old`/`new` with surrounding lines for anchoring) must not render
	// those unchanged lines as a `-`/`+` pair: the common head and tail of
	// `old`/`new` are stripped so only the real change shows as a diff, and the
	// padded lines render as ordinary context. Otherwise the card looks like
	// the edit rewrote (duplicated) far more text than it did.
	it('common prefix/suffix lines of old/new render as context, not remove+add pairs', () => {
		const cache = ['p1', 'p2', 'old-mid', 'p3', 'p4'];
		const out = buildFileDiff(
			cache,
			[{ path: 'f', old: ['p1', 'p2', 'old-mid', 'p3', 'p4'], new: ['p1', 'p2', 'new-mid', 'p3', 'p4'] }],
			1
		);
		expect(out.diff).toBe('@@ -2,3 +2,3 @@\n p2\n-old-mid\n+new-mid\n p3');
	});

	// Prefix-only sharing (an insertion padded with an anchor line AFTER the
	// change): the shared tail renders as context, the inserted head as `+`.
	it('a change with only a shared suffix strips the tail, not the head', () => {
		const cache = ['x', 'anchor'];
		const out = buildFileDiff(
			cache,
			[{ path: 'f', old: ['x', 'anchor'], new: ['X1', 'X2', 'anchor'] }],
			1
		);
		expect(out.diff).toBe('@@ -1,2 +1,3 @@\n-x\n+X1\n+X2\n anchor');
	});

	// Stripping is per-entry: the place-in-file anchor (findMatches against the
	// FULL old) is unaffected, so a stripped splice still lands exactly where
	// the backend applied it — only the rendered hunk narrows.
	it('stripping never moves where the hunk is anchored', () => {
		const cache = ['pre', 'mid-old', 'post', 'other'];
		const out = buildFileDiff(
			cache,
			[{ path: 'f', old: ['pre', 'mid-old', 'post'], new: ['pre', 'mid-new', 'post'] }],
			0
		);
		// context 0 → only the changed line, no padding.
		expect(out.diff).toBe('@@ -2,1 +2,1 @@\n-mid-old\n+mid-new');
	});

	// Identical old/new (a no-op edit) strips to nothing at all: emitting a
	// hunk of pure context with an `@@` header would claim a change happened
	// where none did, so the diff is empty and the card shows no diff block.
	it('identical old and new renders no diff at all', () => {
		const cache = ['a', 'b', 'c'];
		const out = buildFileDiff(cache, [{ path: 'f', old: ['b'], new: ['b'] }], 1);
		expect(out.diff).toBe('');
	});

	it('replace_all replaces every non-overlapping occurrence', () => {
		const cache = ['x', 'y', 'x', 'z', 'x'];
		const out = buildFileDiff(cache, [{ path: 'f', old: ['x'], new: ['X'], replace_all: true }], 1);
		// Three splices merge into one hunk since they're all within 2*context.
		expect(out.diff.match(/@@ /g)?.length).toBe(1);
		expect(out.diff).toContain('-x');
		expect(out.diff).toContain('+X');
		expect(out.note).toBeUndefined();
	});

	it('a cache miss renders a bare block with a note, not a crash', () => {
		const out = buildFileDiff(undefined, [{ path: 'f', old: ['a'], new: ['b'] }]);
		expect(out.diff).toBe('-a\n+b');
		expect(out.note).toBe('无上下文（该文件未在本会话读取）');
	});

	// The real-world case behind the bare-block path: the file CHANGED on disk
	// after it was read into the cache (e.g. another edit or an external
	// write), so the model's quoted `old` no longer matches the cached lines.
	// Even here, lines the entry shares between `old` and `new` must NOT render
	// as `-`/`+` pairs — strip them to context, leaving only the true change.
	it('a stale-cache entry still strips lines shared by old and new', () => {
		const cache = ['anything', 'else']; // entry's old is absent → bare path
		const out = buildFileDiff(
			cache,
			[{ path: 'f', old: ['head', 'mid-old', 'tail'], new: ['head', 'mid-new', 'tail'] }]
		);
		expect(out.diff).toBe(' head\n-mid-old\n+mid-new\n tail');
		expect(out.note).toBe('部分内容未在当前缓存中定位（可能已过时）');
	});

	it('an old that matches nowhere in the cache is a stale preview, not a crash', () => {
		const cache = ['a', 'b', 'c'];
		const out = buildFileDiff(cache, [{ path: 'f', old: ['zzz'], new: ['Z'] }]);
		expect(out.diff).toBe('-zzz\n+Z');
		expect(out.note).toBe('部分内容未在当前缓存中定位（可能已过时）');
	});

	it('an ambiguous match previews the first occurrence and flags it', () => {
		const cache = ['x', 'y', 'x'];
		const out = buildFileDiff(cache, [{ path: 'f', old: ['x'], new: ['X'] }]);
		expect(out.diff).toBe('@@ -1,3 +1,3 @@\n-x\n+X\n y\n x');
		expect(out.note).toBe('预览取首个匹配，以执行结果为准');
	});
});

describe('buildWriteDiff', () => {
	it('renders no hunk for identical content', () => {
		const lines = ['a', 'b', 'c'];
		const out = buildWriteDiff(lines, [...lines]);
		expect(out.diff).toBe('');
	});

	it('renders one hunk with context for a single-line change', () => {
		const oldLines = ['a', 'b', 'c', 'd', 'e'];
		const newLines = ['a', 'b', 'C', 'd', 'e'];
		const out = buildWriteDiff(oldLines, newLines, 1);
		expect(out.diff).toBe('@@ -2,3 +2,3 @@\n b\n-c\n+C\n d');
	});

	it('splits distant changes into two hunks', () => {
		const oldLines = Array.from({ length: 12 }, (_, i) => `l${i + 1}`);
		const newLines = [...oldLines];
		newLines[1] = 'A'; // l2 -> A
		newLines[10] = 'B'; // l11 -> B
		const out = buildWriteDiff(oldLines, newLines, 1);
		expect(out.diff).toBe(
			'@@ -1,3 +1,3 @@\n l1\n-l2\n+A\n l3\n@@ -10,3 +10,3 @@\n l10\n-l11\n+B\n l12'
		);
	});

	it('merges close changes into one hunk', () => {
		const oldLines = ['a', 'b', 'c', 'd', 'e'];
		const newLines = ['a', 'B', 'c', 'D', 'e'];
		const out = buildWriteDiff(oldLines, newLines, 3);
		expect(out.diff).toBe('@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n-d\n+D\n e');
	});

	it('renders a pure line-level diff, not a whole-block replace (unlike edit splices)', () => {
		// Only line 2 actually changed; the real diff should say so instead of
		// treating the whole range as one -old/+new block.
		const oldLines = ['a', 'b', 'c'];
		const newLines = ['a', 'B', 'c'];
		const out = buildWriteDiff(oldLines, newLines, 3);
		expect(out.diff).toBe('@@ -1,3 +1,3 @@\n a\n-b\n+B\n c');
	});

	it('renders a pure insertion with no old-side line at the hunk start', () => {
		const oldLines = ['a', 'b'];
		const newLines = ['x', 'a', 'b'];
		const out = buildWriteDiff(oldLines, newLines, 1);
		// context=1: change at flat[0], window [0,2) → +x, a — only 1 context line after the insert.
		expect(out.diff).toBe('@@ -1,1 +1,2 @@\n+x\n a');
	});
});

describe('parsePartialEdits', () => {
	it('returns nothing before the edits key has arrived', () => {
		expect(parsePartialEdits('{"e')).toEqual([]);
	});

	it('returns nothing while the first entry is still open', () => {
		expect(parsePartialEdits('{"edits":[{"path":"a","old":["x"')).toEqual([]);
	});

	it('extracts a fully-closed entry while the next one is still streaming', () => {
		const args = '{"edits":[{"path":"a","old":["x"],"new":["y"]},{"path":"b","old":["z"';
		expect(parsePartialEdits(args)).toEqual([{ path: 'a', old: ['x'], new: ['y'] }]);
	});

	it('parses a fully-formed edits array, including replace_all', () => {
		const obj = {
			edits: [
				{ path: 'a', old: ['x'], new: ['y'] },
				{ path: 'b', old: ['p'], new: ['q'], replace_all: true }
			]
		};
		expect(parsePartialEdits(JSON.stringify(obj))).toEqual(obj.edits);
	});

	it('does not miscount braces inside a quoted line', () => {
		const obj = { edits: [{ path: 'a', old: ['x{y}z'], new: ['w'] }] };
		expect(parsePartialEdits(JSON.stringify(obj))).toEqual(obj.edits);
	});

	it('handles an escaped quote inside a quoted line', () => {
		const obj = { edits: [{ path: 'a', old: ['say "hi"'], new: ['ok'] }] };
		expect(parsePartialEdits(JSON.stringify(obj))).toEqual(obj.edits);
	});
});
