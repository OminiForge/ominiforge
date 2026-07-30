import { describe, it, expect, beforeEach } from 'vitest';
import { applyHubStatus, notifySessionEvent, viewState } from './status.svelte';
import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { SessionStatus } from '$lib/types/SessionStatus';

// The status module keeps per-session state in module-level maps keyed by id,
// so each test uses a fresh session id instead of resetting internals.

function turnStarted(sessionId: string, seq: number): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq: BigInt(seq),
		session_id: sessionId,
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'User', id: 'u' },
		parent_event_id: null,
		turn_id: null,
		payload: { Turn: { Started: { turn_id: 't1', input: 'hi' } } }
	};
}

function hubStatus(sessionId: string, status: SessionStatus['status'], seq: number): SessionStatus {
	return { session_id: sessionId, workspace_id: 'none', status, latest_seq: BigInt(seq) };
}

beforeEach(() => {
	// Node test env has no localStorage (the module guards its access); clear
	// it when present so the persisted ack/seq maps never leak between tests.
	globalThis.localStorage?.clear();
});

describe('notifySessionEvent (turn-open → optimistic running)', () => {
	it('flips an idle session row to running the moment its turn opens', () => {
		// WHY: the gateway publishes `running` only when the actor DEQUEUES the
		// send, which can lag the committed Turn::Started arbitrarily (e.g. the
		// actor is parked on an approval from the previous turn). If the row
		// doesn't flip here, the user sees "done" for a turn that's already
		// running — the stuck-on-idle bug this closes.
		const id = 't-flip';
		expect(viewState(id)).toBe('seen'); // unknown session: resting
		notifySessionEvent(turnStarted(id, 5));
		expect(viewState(id)).toBe('running');
	});

	it('ignores non-Started turn events', () => {
		const id = 't-ignore';
		const completed = turnStarted(id, 3);
		if (completed.type === 'event') {
			completed.payload = { Turn: { Completed: { turn_id: 't1' } } };
		}
		notifySessionEvent(completed);
		expect(viewState(id)).not.toBe('running');
	});
});

describe('applyHubStatus (optimistic running vs the hub)', () => {
	it('ignores a stale idle snapshot that predates the turn-open', () => {
		// WHY: a reconnect re-snapshots the hub, which can still hold the pre-turn
		// idle (the actor hasn't dequeued yet). If that snapshot overwrote the
		// optimistic running, the row would flap back to "done" mid-turn.
		const id = 't-stale';
		applyHubStatus(hubStatus(id, 'idle', 2));
		notifySessionEvent(turnStarted(id, 5));
		applyHubStatus(hubStatus(id, 'idle', 2)); // stale snapshot, seq < turn-open
		expect(viewState(id)).toBe('running');
	});

	it('lets the real settle idle through even if the running delta was lost', () => {
		// WHY: the settle idle carries latest_seq >= the turn-open seq. Dropping
		// it (as a blanket idle-ignore would) wedges the row on running forever
		// whenever the authoritative running delta never arrived (lagged
		// broadcast, mid-flight connect) — the opposite stuck-state bug.
		const id = 't-settle';
		applyHubStatus(hubStatus(id, 'idle', 2));
		notifySessionEvent(turnStarted(id, 5));
		applyHubStatus(hubStatus(id, 'idle', 6)); // turn settled; no running delta seen
		expect(viewState(id)).not.toBe('running');
	});

	it('still applies ordinary last-write-wins transitions', () => {
		const id = 't-lww';
		applyHubStatus(hubStatus(id, 'running', 4));
		expect(viewState(id)).toBe('running');
		applyHubStatus(hubStatus(id, 'idle', 9));
		expect(viewState(id)).not.toBe('running');
	});
});
