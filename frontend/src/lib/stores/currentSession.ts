import { writable } from 'svelte/store';
import type { SessionMeta } from '$lib/types/SessionMeta';

/**
 * The session the user is currently viewing, or `null` when not on a session
 * page. The sidebar RUNTIME panel reads this to show workspace / model /
 * profile / env for the active session; when `null` the panel is hidden.
 *
 * `sessions/[id]` writes its meta here on load and clears it on destroy so the
 * panel never shows stale context on the list / monitor / evolution pages.
 */
export const currentSession = writable<SessionMeta | null>(null);
