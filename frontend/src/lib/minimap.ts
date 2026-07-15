// Pure geometry for the conversation minimap (jump-to-user-message rail). The
// DOM measurement + scrolling live in Conversation.svelte; the position math is
// factored here so it can be unit-tested without a browser (the screenshot tool
// can't render the workspace route). See queue.ts for the same split rationale.

/** One user message's normalized position on the scroll rail: `top` is its
 *  fraction (0..1) of the full scroll content height; `index` is its items[]
 *  index (the jump target). */
export interface Tick {
	index: number;
	top: number;
}

/** The tick the viewport currently sits at: the last one whose fraction is at or
 *  above the scroll position, within a small tolerance so a tick sitting exactly
 *  at the viewport top counts as active. Returns the tick's `index`, or -1 when
 *  none qualifies (scrolled above the first). Assumes `ticks` in document order.
 *
 *  `scrollFrac` must share the ticks' basis (fraction of full scrollHeight). */
export function activeTick(ticks: Tick[], scrollFrac: number, tolerance = 0.02): number {
	let active = -1;
	for (const t of ticks) {
		if (t.top <= scrollFrac + tolerance) active = t.index;
		else break;
	}
	return active;
}

/** The next/previous user-message scroll offset relative to `scrollTop`, in
 *  pixels, or `null` when there is none in that direction. `dir` is +1 (down /
 *  next) or -1 (up / previous). `eps` skips the message we're already parked on
 *  so a repeated press advances instead of re-selecting it.
 *
 *  `total` is the scroll content height (scrollHeight); tick fractions are scaled
 *  by it to absolute offsets. */
export function jumpTarget(
	ticks: Tick[],
	scrollTop: number,
	total: number,
	dir: 1 | -1,
	eps = 8
): number | null {
	if (ticks.length === 0) return null;
	const positions = ticks.map((t) => t.top * total).sort((a, b) => a - b);
	if (dir === 1) {
		return positions.find((p) => p > scrollTop + eps) ?? null;
	}
	let target: number | null = null;
	for (const p of positions) {
		if (p < scrollTop - eps) target = p;
		else break;
	}
	return target;
}
