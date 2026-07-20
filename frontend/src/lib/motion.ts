import { cubicOut } from 'svelte/easing';

/**
 * Shared Svelte-transition parameters (DESIGN.md §3.2). Durations come only
 * from the 120/200ms motion scale; every factory returns duration 0 under
 * `prefers-reduced-motion` so JS transitions degrade exactly like CSS motion.
 * Params are evaluated when the transition triggers, so a `rise()` literal in
 * markup always picks up the current motion preference.
 */

function reduced(): boolean {
	return matchMedia('(prefers-reduced-motion: reduce)').matches;
}

function dur(ms: number): number {
	return reduced() ? 0 : ms;
}

/** fly params: enters sliding from a `y`-px offset (positive = from below).
 *  `delay` staggers entrances (e.g. card grids); it also zeroes under
 *  reduced-motion, where a delayed instant appearance is pure latency. */
export function rise(y = 6, ms = 120, delay = 0) {
	return { y, duration: dur(ms), delay: reduced() ? 0 : delay, easing: cubicOut };
}

/** scale params: pops in from slightly smaller. */
export function pop(ms = 200) {
	return { start: 0.96, duration: dur(ms), easing: cubicOut };
}

/** fade params: plain opacity crossfade. */
export function fadeIn(ms = 120) {
	return { duration: dur(ms) };
}
