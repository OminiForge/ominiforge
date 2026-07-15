// Per-session outgoing message queue. When the user sends while a turn is
// running, the gateway would accept the message but silently defer it in an
// in-memory VecDeque (src/gateway/actor.rs) — invisible, and impossible to
// cancel. This module holds those messages on the CLIENT instead: they show as
// pending chips the user can cancel (recovering the text back into the input),
// and the conversation view flushes them one at a time as each turn settles.
//
// Persisted to localStorage per session id so a refresh mid-turn doesn't drop
// unsent text. Pure data + storage helpers here (unit-tested in queue.test.ts);
// the component owns the flush-on-settle wiring.

/** One queued, not-yet-sent user message. `id` is a stable local key for the
 *  {#each} list and for targeted removal — it never reaches the backend. */
export interface QueuedMessage {
	id: string;
	text: string;
}

const KEY_PREFIX = 'ominiforge.queue.v1.';

function storageKey(sessionId: string): string {
	return KEY_PREFIX + sessionId;
}

/** Load a session's queued messages, or `[]` when none / on any parse error.
 *  SSR-guarded (no `localStorage` on the server), matching status.svelte.ts. */
export function loadQueue(sessionId: string): QueuedMessage[] {
	if (typeof localStorage === 'undefined') return [];
	try {
		const raw = localStorage.getItem(storageKey(sessionId));
		if (!raw) return [];
		const parsed = JSON.parse(raw) as unknown;
		// Defensive: only accept the expected shape so a corrupt/foreign value
		// can't inject malformed items into the send path.
		if (!Array.isArray(parsed)) return [];
		return parsed.filter(
			(m): m is QueuedMessage =>
				!!m &&
				typeof (m as QueuedMessage).id === 'string' &&
				typeof (m as QueuedMessage).text === 'string'
		);
	} catch {
		return [];
	}
}

/** Persist a session's queue. An empty queue removes the key rather than
 *  storing `[]`, so drained sessions don't accumulate dead entries. */
export function saveQueue(sessionId: string, queue: QueuedMessage[]): void {
	if (typeof localStorage === 'undefined') return;
	try {
		if (queue.length === 0) {
			localStorage.removeItem(storageKey(sessionId));
		} else {
			localStorage.setItem(storageKey(sessionId), JSON.stringify(queue));
		}
	} catch {
		// storage full / disabled — the in-memory queue still works this session.
	}
}

/** Append `text` as a new queued message, returning a NEW array (callers assign
 *  it to `$state` so Svelte re-renders). `id` is derived from a monotonic
 *  counter seeded off the current queue so ids stay unique without Date.now()/
 *  Math.random(). Whitespace-only text is rejected (nothing to send). */
export function enqueue(queue: QueuedMessage[], text: string): QueuedMessage[] {
	const trimmed = text.trim();
	if (!trimmed) return queue;
	return [...queue, { id: nextId(queue), text: trimmed }];
}

/** Remove the message with `id`, returning a NEW array. */
export function removeFromQueue(queue: QueuedMessage[], id: string): QueuedMessage[] {
	return queue.filter((m) => m.id !== id);
}

/** Next unique id: one past the largest numeric id present. Stable across a
 *  session (mirrors the plan-step id scheme in conversation.ts) and free of the
 *  non-deterministic clocks the workflow sandbox forbids. */
function nextId(queue: QueuedMessage[]): string {
	const max = queue.reduce((m, item) => {
		const n = Number(item.id);
		return Number.isInteger(n) && n > m ? n : m;
	}, 0);
	return String(max + 1);
}
