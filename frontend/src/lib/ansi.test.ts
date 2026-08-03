import { describe, it, expect } from 'vitest';
import { parseAnsi } from './ansi';

describe('parseAnsi', () => {
	it('returns a single unstyled segment for plain text', () => {
		expect(parseAnsi('hello world')).toEqual([{ text: 'hello world' }]);
	});

	it('parses a simple color', () => {
		const segs = parseAnsi('\u001b[31merror\u001b[0m ok');
		expect(segs).toEqual([
			{ text: 'error', fg: 'var(--state-error-text)' },
			{ text: ' ok' }
		]);
	});

	it('parses bold', () => {
		const segs = parseAnsi('\u001b[1mbold\u001b[22mnormal');
		expect(segs).toEqual([{ text: 'bold', bold: true }, { text: 'normal' }]);
	});

	it('parses combined attributes', () => {
		const segs = parseAnsi('\u001b[1;33mwarn\u001b[0m');
		expect(segs).toEqual([{ text: 'warn', bold: true, fg: 'var(--state-running-text)' }]);
	});

	it('handles color reset (39) without touching other attributes', () => {
		const segs = parseAnsi('\u001b[1;31mred bold\u001b[39m just bold');
		expect(segs).toEqual([
			{ text: 'red bold', fg: 'var(--state-error-text)', bold: true },
			{ text: ' just bold', bold: true }
		]);
	});

	it('strips non-SGR CSI sequences (cursor movement)', () => {
		expect(parseAnsi('\u001b[2Aup\u001b[Kclear')).toEqual([{ text: 'upclear' }]);
	});

	it('strips OSC sequences (hyperlinks, titles)', () => {
		expect(parseAnsi('\u001b]8;;https://example.com\u0007link\u001b]8;;\u0007')).toEqual([
			{ text: 'link' }
		]);
	});

	it('handles the direnv banner pattern from the bug report', () => {
		const segs = parseAnsi('\u001b[0mdirenv: loading ~/project/.envrc\n\u001b[0mdirenv: using flake');
		expect(segs).toEqual([
			{ text: 'direnv: loading ~/project/.envrc\n' },
			{ text: 'direnv: using flake' }
		]);
	});

	it('handles empty input', () => {
		expect(parseAnsi('')).toEqual([]);
	});

	it('ignores unsupported SGR codes gracefully', () => {
		const segs = parseAnsi('\u001b[5mblink\u001b[0m');
		expect(segs).toEqual([{ text: 'blink' }]);
	});
});
