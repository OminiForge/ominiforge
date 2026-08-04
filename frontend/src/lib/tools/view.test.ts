import { describe, expect, it } from 'vitest';
import { parseView } from './view';

/** The `formatted_by` / `format_adjustments` annotation is the contract that
 *  lets the diff view say "part of this change is the formatter's, not the
 *  model's" (`doc/format.md` §6). These tests pin that the backend's JSON
 *  envelope parses into the fields the renderer reads — if the wire shape
 *  drifts, the annotation silently disappears and the model's edit is
 *  misattributed as its own. */
describe('parseView diff annotation', () => {
	it('parses formatted_by and format_adjustments when present', () => {
		const view = parseView(
			JSON.stringify({
				kind: 'diff',
				files: [
					{
						path: 'src/a.rs',
						patch: '@@ -1,1 +1,1 @@\n-a\n+A',
						formatted_by: 'rustfmt',
						format_adjustments: 2
					}
				]
			})
		);
		expect(view?.kind).toBe('diff');
		if (view?.kind !== 'diff') return;
		expect(view.files[0].formatted_by).toBe('rustfmt');
		expect(view.files[0].format_adjustments).toBe(2);
	});

	it('leaves the annotation undefined for a plain diff (no formatter ran)', () => {
		const view = parseView(
			JSON.stringify({
				kind: 'diff',
				files: [{ path: 'src/a.rs', patch: '@@ -1,1 +1,1 @@\n-a\n+A' }]
			})
		);
		if (view?.kind !== 'diff') throw new Error('expected diff');
		expect(view.files[0].formatted_by).toBeUndefined();
		expect(view.files[0].format_adjustments).toBeUndefined();
	});

	it('drops malformed annotation fields rather than guessing', () => {
		const view = parseView(
			JSON.stringify({
				kind: 'diff',
				files: [
					{
						path: 'src/a.rs',
						patch: 'x',
						formatted_by: 42,
						format_adjustments: 'two'
					}
				]
			})
		);
		if (view?.kind !== 'diff') throw new Error('expected diff');
		expect(view.files[0].formatted_by).toBeUndefined();
		expect(view.files[0].format_adjustments).toBeUndefined();
	});
});
