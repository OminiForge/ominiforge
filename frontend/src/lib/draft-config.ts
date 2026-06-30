// Hand-off for a draft session's preselected config from the dashboard's
// "New session ▾" split-button to the draft conversation page. The dashboard
// can't pass props across a `goto('/sessions/new')` navigation, so the choice
// rides through sessionStorage: written on navigate, read-and-cleared once when
// the draft mounts. Cleared after reading so a later plain "New session" starts
// on gateway defaults rather than inheriting a stale choice.

import type { CreateSessionOptions } from './client-core';

/** sessionStorage key holding the pending draft config (JSON-encoded
 *  {@link CreateSessionOptions}). */
export const DRAFT_CONFIG_KEY = 'omini.draftConfig';

/** Stash a draft config for the next `/sessions/new` open. A no-op when nothing
 *  was chosen, so a plain "New session" leaves no stale entry. */
export function stashDraftConfig(opts: CreateSessionOptions): void {
	if (!opts.profile && !opts.model && !opts.workspace) return;
	try {
		sessionStorage.setItem(DRAFT_CONFIG_KEY, JSON.stringify(opts));
	} catch {
		/* sessionStorage unavailable (private mode / SSR): skip — the draft just
		   opens on defaults. */
	}
}

/** Read-and-clear the stashed draft config. Returns `{}` when none is pending.
 *  Clearing on read makes the hand-off one-shot. */
export function takeDraftConfig(): CreateSessionOptions {
	try {
		const raw = sessionStorage.getItem(DRAFT_CONFIG_KEY);
		if (!raw) return {};
		sessionStorage.removeItem(DRAFT_CONFIG_KEY);
		return JSON.parse(raw) as CreateSessionOptions;
	} catch {
		return {};
	}
}
