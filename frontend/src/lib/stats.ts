// Shared session-stat formatting. Single source of truth for the metric labels
// and value formatting shown on the Dashboard cards and the session detail rail,
// so a wording/format change (e.g. "tool call" → something else) lands in one
// place instead of being duplicated per page.

import type { SessionSummary } from '$lib/types/SessionSummary';

/** u64 fields arrive as JS number|bigint over JSON; coerce defensively. */
export const num = (v: number | bigint): number => Number(v);

/** English plural for a count label: `1 turn`, `2 turns`. Stat nouns are all
 *  regular, so a trailing `s` is the only rule needed. */
export function plural(n: number, word: string): string {
	return n === 1 ? word : `${word}s`;
}

/** Metric labels, keyed by stat. Count-based labels take the count so they can
 *  pluralize; fixed labels are plain strings. Both pages read these, so the
 *  wording is defined exactly once. */
export const statLabel = {
	turns: (n: number) => plural(n, 'turn'),
	reqs: (n: number) => plural(n, 'req'),
	toolCalls: (n: number) => plural(n, 'tool call'),
	inTok: 'in tok',
	outTok: 'out tok',
	cache: 'cache'
} as const;

/** Cache-hit rate as a whole-percent string. Branched sessions carry an
 *  inherited snapshot their own requests never re-read, so their rate
 *  reflects only this session's requests (reads low right after a branch) —
 *  the `*` marks that caveat (details on hover). */
export function cacheLabel(s: SessionSummary, branched: boolean): string {
	return `${(s.cache_hit_rate * 100).toFixed(0)}%${branched ? '*' : ''}`;
}

/** Top tools by call count, capped. Bar width is relative to this set's own max,
 *  so each panel reads on its own scale (per-session breakdowns, not comparable
 *  across panels). */
export function topTools(
	s: SessionSummary,
	cap: number
): { tool: string; count: number; pct: number }[] {
	const entries = Object.entries(s.tools_used)
		.map(([tool, c]) => ({ tool, count: num(c) }))
		.sort((a, b) => b.count - a.count);
	const max = Math.max(1, ...entries.map((e) => e.count));
	return entries.slice(0, cap).map((e) => ({ ...e, pct: (e.count / max) * 100 }));
}
