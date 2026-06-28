import { writable } from 'svelte/store';
import type { SessionMeta } from '$lib/types/SessionMeta';
import type { RuntimeInfo } from '$lib/types/RuntimeInfo';

/**
 * The session the user is currently viewing, or `null` when not on a session
 * page. The sidebar RUNTIME panel reads this to show workspace / model /
 * profile / env for the active session; when `null` the panel is hidden.
 *
 * `sessions/[id]` writes its meta here on load and clears it on destroy so the
 * panel never shows stale context on the list / monitor / evolution pages.
 */
export const currentSession = writable<SessionMeta | null>(null);

/**
 * The config-layer provider/model resolved for the current session, or `null`
 * when unknown (not on a session page, or the runtime lookup failed). The
 * RUNTIME panel's Model row reads this; it is set/cleared alongside
 * [`currentSession`] so the two never disagree about which session is active.
 */
export const currentRuntime = writable<RuntimeInfo | null>(null);

/**
 * Distinct models the current session's runtime layer actually used (from
 * `RequestStarted` events, folded in `conversation.ts`). The RUNTIME panel
 * compares these against the config-layer model ([`currentRuntime`]) and fails
 * loud on divergence — a subagent/fork running a different model than the one
 * configured (`doc/frontend.md` B4, CLAUDE.md #12). Empty off a session page.
 *
 * This is a *validation* source only; it never drives the displayed Model row,
 * so that row stays stable while subagents switch models.
 */
export const currentRuntimeModels = writable<string[]>([]);
