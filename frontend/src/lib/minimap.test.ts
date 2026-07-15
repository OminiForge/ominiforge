import { describe, it, expect } from 'vitest';
import { activeTick, jumpTarget, type Tick } from './minimap';

// Three user messages at 0%, 40%, 80% of the scroll content.
const TICKS: Tick[] = [
	{ index: 0, top: 0 },
	{ index: 3, top: 0.4 },
	{ index: 7, top: 0.8 }
];

describe('activeTick', () => {
	it('highlights the message the viewport has scrolled to', () => {
		// WHY: the active tick tells the user "you are reading THIS message" — it
		// must be the last one at/above the viewport top, not the nearest.
		expect(activeTick(TICKS, 0)).toBe(0);
		expect(activeTick(TICKS, 0.5)).toBe(3);
		expect(activeTick(TICKS, 0.9)).toBe(7);
	});

	it('returns -1 when scrolled above the first message', () => {
		// WHY: with content padding above the first message, nothing is active yet;
		// -1 means "no highlight" rather than falsely lighting the first tick.
		expect(activeTick([{ index: 2, top: 0.3 }], 0.1)).toBe(-1);
	});

	it('counts a tick sitting exactly at the viewport top as active', () => {
		// WHY: after a jump lands a message flush at the top, its tick must read as
		// active — the tolerance absorbs sub-pixel/rounding drift.
		expect(activeTick(TICKS, 0.4 - 0.01)).toBe(3);
	});
});

describe('jumpTarget', () => {
	const TOTAL = 1000; // scrollHeight → ticks at 0, 400, 800 px

	it('goes to the next message below the current position', () => {
		// WHY: Ctrl+↓ / clicking down must advance to the next user turn, not the
		// current one.
		expect(jumpTarget(TICKS, 0, TOTAL, 1)).toBe(400);
		expect(jumpTarget(TICKS, 400, TOTAL, 1)).toBe(800);
	});

	it('goes to the previous message above the current position', () => {
		// WHY: Ctrl+↑ must step back one user turn.
		expect(jumpTarget(TICKS, 800, TOTAL, -1)).toBe(400);
		expect(jumpTarget(TICKS, 400, TOTAL, -1)).toBe(0);
	});

	it('returns null at the ends so navigation never wraps or jitters', () => {
		// WHY: past the last message there is no "next"; wrapping to the top would
		// disorient. Same for "previous" above the first.
		expect(jumpTarget(TICKS, 800, TOTAL, 1)).toBeNull();
		expect(jumpTarget(TICKS, 0, TOTAL, -1)).toBeNull();
	});

	it('skips the message we are already parked on (eps) so repeats advance', () => {
		// WHY: landing exactly on a message then pressing again must move to the
		// NEXT one, not re-select the current — otherwise Ctrl+↓ gets stuck.
		expect(jumpTarget(TICKS, 400 + 2, TOTAL, 1)).toBe(800);
		expect(jumpTarget(TICKS, 400 - 2, TOTAL, -1)).toBe(0);
	});

	it('is a no-op with no ticks', () => {
		expect(jumpTarget([], 0, TOTAL, 1)).toBeNull();
	});
});
