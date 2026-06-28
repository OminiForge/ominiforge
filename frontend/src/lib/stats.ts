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
	cost: 'cost',
	inTok: 'in tok',
	outTok: 'out tok',
	cache: 'cache'
} as const;

/** Display cost: `$1.23`, `$0.0042` for sub-cent, or `unpriced` when no priced
 *  model ran (so the UI never prints a misleading `$0.00`). */
export function formatCost(s: SessionSummary): string {
	if (s.cost_usd == null) return 'unpriced';
	return `$${s.cost_usd.toFixed(s.cost_usd < 0.01 ? 4 : 2)}`;
}

/** Cache-hit rate as a whole-percent string. */
export function cacheLabel(s: SessionSummary): string {
	return `${(s.cache_hit_rate * 100).toFixed(0)}%`;
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
