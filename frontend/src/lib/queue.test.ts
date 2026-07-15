import { describe, it, expect, beforeEach } from 'vitest';
import {
	loadQueue,
	saveQueue,
	enqueue,
	removeFromQueue,
	type QueuedMessage
} from './queue';

// A minimal in-memory localStorage stand-in, so the persistence contract can be
// asserted without jsdom. Installed on globalThis before each test.
function installStorage(): Storage {
	const map = new Map<string, string>();
	const storage = {
		getItem: (k: string) => (map.has(k) ? map.get(k)! : null),
		setItem: (k: string, v: string) => void map.set(k, v),
		removeItem: (k: string) => void map.delete(k),
		clear: () => map.clear(),
		key: (i: number) => [...map.keys()][i] ?? null,
		get length() {
			return map.size;
		}
	} as Storage;
	(globalThis as { localStorage: Storage }).localStorage = storage;
	return storage;
}

describe('queue', () => {
	beforeEach(() => {
		installStorage();
	});

	it('persists across a reload so a mid-turn refresh does not drop unsent text', () => {
		// WHY: the whole point of persisting is that closing/refreshing the tab
		// while a turn runs must not lose a queued message. Simulate that by
		// saving under one "page load" and loading fresh under another.
		const q = enqueue([], 'follow-up while the agent is busy');
		saveQueue('sess-A', q);

		const reloaded = loadQueue('sess-A');
		expect(reloaded).toHaveLength(1);
		expect(reloaded[0].text).toBe('follow-up while the agent is busy');
	});

	it('scopes the queue per session id', () => {
		// WHY: two sessions must not share a pending queue — a message queued in
		// session A must never flush into session B.
		saveQueue('sess-A', enqueue([], 'for A'));
		saveQueue('sess-B', enqueue([], 'for B'));
		expect(loadQueue('sess-A')[0].text).toBe('for A');
		expect(loadQueue('sess-B')[0].text).toBe('for B');
	});

	it('draining the queue clears the storage key rather than storing []', () => {
		// WHY: a resting session should leave no dead localStorage entry, so the
		// key namespace does not grow unbounded across many finished sessions.
		const storage = installStorage();
		saveQueue('sess-A', enqueue([], 'one'));
		expect(storage.getItem('ominiforge.queue.v1.sess-A')).not.toBeNull();
		saveQueue('sess-A', []);
		expect(storage.getItem('ominiforge.queue.v1.sess-A')).toBeNull();
	});

	it('cancel removes exactly the targeted message, leaving the rest ordered', () => {
		// WHY: cancelling one pending chip must not disturb the others or their
		// send order — the queue flushes FIFO.
		let q: QueuedMessage[] = [];
		q = enqueue(q, 'first');
		q = enqueue(q, 'second');
		q = enqueue(q, 'third');
		const middle = q[1].id;
		q = removeFromQueue(q, middle);
		expect(q.map((m) => m.text)).toEqual(['first', 'third']);
	});

	it('assigns unique ids even after removals so list keys never collide', () => {
		// WHY: ids key the {#each} list and targeted removal; a reused id after a
		// cancel would corrupt which chip the next × removes.
		let q: QueuedMessage[] = [];
		q = enqueue(q, 'a');
		q = enqueue(q, 'b');
		q = removeFromQueue(q, q[0].id);
		q = enqueue(q, 'c');
		const ids = q.map((m) => m.id);
		expect(new Set(ids).size).toBe(ids.length);
	});

	it('trims text and rejects whitespace-only sends', () => {
		// WHY: a blank enqueue would create an un-sendable chip and (once flushed)
		// a no-op turn; the send path already trims, so the queue must too.
		expect(enqueue([], '   ')).toHaveLength(0);
		expect(enqueue([], '  hi  ')[0].text).toBe('hi');
	});

	it('returns an empty queue for a corrupt stored value instead of throwing', () => {
		// WHY: a foreign/corrupt localStorage value must degrade to "no queue",
		// never crash the conversation view on load.
		const storage = installStorage();
		storage.setItem('ominiforge.queue.v1.sess-A', '{not json');
		expect(loadQueue('sess-A')).toEqual([]);
		storage.setItem('ominiforge.queue.v1.sess-B', '{"nope":1}');
		expect(loadQueue('sess-B')).toEqual([]);
	});
});
