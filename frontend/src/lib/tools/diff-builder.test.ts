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

	it('an insert keeps the anchor line in both old and new (rendered as a whole-block replace, not a sub-line match)', () => {
		const cache = ['a', 'b'];
		const out = buildFileDiff(cache, [{ path: 'f', old: ['a'], new: ['a', 'A1'] }], 1);
		// The splice is one mechanical block: the whole `old` range is `-`, the
		// whole `new` payload is `+` — the renderer doesn't sub-diff a splice's
		// payload against what it replaced.
		expect(out.diff).toBe('@@ -1,2 +1,3 @@\n-a\n+a\n+A1\n b');
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
