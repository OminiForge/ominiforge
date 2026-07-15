// Shared, app-level session activity status — one SSE subscription for the whole
// app, consumed by every session-list instance. Backend owns the machine status
// (running / awaiting_approval / idle); this layer adds the client-only
// seen/unseen split by comparing each idle session's latest committed seq against
// a locally-remembered "acknowledged" seq (persisted to localStorage), because
// the gateway cannot know what a user has actually looked at.
//
// `.svelte.ts` so the `$state` runes below are reactive when imported into
// components (Svelte 5 universal reactivity).

import { client } from '$lib/client';
import type { SessionStatus } from '$lib/types/SessionStatus';
import type { EventSubscription } from '$lib/client-core';

/** What a session-list row should render. `unseen`/`seen` are the two client-side
 *  refinements of the backend's `idle`. */
export type ViewState = 'running' | 'awaiting' | 'unseen' | 'seen';

const ACK_KEY = 'ominiforge.seen.v1';
const SEQ_KEY = 'ominiforge.latestSeq.v1';

/** Last-known status per session, keyed by session id, last-write-wins from the
 *  status stream. Reactive: reads in components re-run on update. */
const statuses = $state<Record<string, SessionStatus>>({});

/** Acknowledged (seen-up-to) seq per session, persisted to localStorage. A row is
 *  "unseen" only when its latest committed seq exceeds this. */
const acked = $state<Record<string, number>>(loadAcked());

/** Last-known `latest_seq` per session, persisted to localStorage. Survives page
 *  refreshes and gateway restarts so the unseen→seen split works even when the SSE
 *  status snapshot doesn't include a given session (cold session, restarted hub).
 *  Updated on every status delta; monotonic (a smaller seq never lowers it). */
const latestSeqs = $state<Record<string, number>>(loadLatestSeqs());

let sub: EventSubscription | null = null;
let refCount = 0;

function loadAcked(): Record<string, number> {
	if (typeof localStorage === 'undefined') return {};
	try {
		const raw = localStorage.getItem(ACK_KEY);
		return raw ? (JSON.parse(raw) as Record<string, number>) : {};
	} catch {
		return {};
	}
}

function persistAcked() {
	if (typeof localStorage === 'undefined') return;
	try {
		localStorage.setItem(ACK_KEY, JSON.stringify(acked));
	} catch {
		// storage full / disabled — the in-memory map still works for this session.
	}
}

function loadLatestSeqs(): Record<string, number> {
	if (typeof localStorage === 'undefined') return {};
	try {
		const raw = localStorage.getItem(SEQ_KEY);
		return raw ? (JSON.parse(raw) as Record<string, number>) : {};
	} catch {
		return {};
	}
}

function persistLatestSeqs() {
	if (typeof localStorage === 'undefined') return;
	try {
		localStorage.setItem(SEQ_KEY, JSON.stringify(latestSeqs));
	} catch {
		// storage full / disabled — the in-memory map still works for this session.
	}
}

/** Start the shared status subscription if it isn't already running, and return a
 *  disposer. Ref-counted so multiple list instances share ONE SSE connection; the
 *  stream closes when the last consumer disposes. Call from a component's
 *  `onMount` and invoke the returned disposer on cleanup. */
export function connectStatus(): () => void {
	refCount += 1;
	if (!sub) {
		sub = client.subscribeStatus({
			onStatus(s) {
				// Last-write-wins by session id; `latest_seq` arrives as a JS number
				// over the wire (ts-rs types it bigint, but serde_json emits a number).
				statuses[s.session_id] = s;

				// Persist latest_seq so the unseen→seen split survives page refreshes.
				const seq = Number(s.latest_seq) || 0;
				if (seq > (latestSeqs[s.session_id] ?? 0)) {
					latestSeqs[s.session_id] = seq;
					persistLatestSeqs();
				}
			}
			// onError omitted: the transport reconnects (and re-snapshots) on its own;
			// a dropped stream simply leaves the last-known statuses in place until it
			// resumes — no stuck spinner because the backend re-publishes on reconnect.
		});
	}
	return () => {
		refCount -= 1;
		if (refCount <= 0 && sub) {
			sub.close();
			sub = null;
			refCount = 0;
		}
	};
}

/** Mark a session seen up to `seq` (the latest committed event the user has now
 *  viewed). Bumps the persisted ack so the row flips unseen→seen. Monotonic — a
 *  smaller seq never lowers the watermark. Called by the conversation view as its
 *  `lastSeq` advances while focused. */
export function markSeen(sessionId: string, seq: number): void {
	if (!Number.isFinite(seq)) return;
	const cur = acked[sessionId] ?? -1;
	if (seq > cur) {
		acked[sessionId] = seq;
		persistAcked();
	}
}

/** The view state for one session row.
 *
 *  `seed` is the row's own latest known seq, used so a session that hasn't
 *  streamed a status this run — but whose log has advanced past what the user
 *  acked — can still read as unseen. When callers don't supply a seed, the
 *  persisted `latestSeqs` map (fed by the live status stream across app runs)
 *  provides a fallback. A session absent from both maps is `seen` (a resting
 *  session with no live actor is never running, and unseen needs a known latest
 *  seq to mean anything). */
export function viewState(sessionId: string, seed?: number): ViewState {
	const s = statuses[sessionId];
	const persistedSeed = latestSeqs[sessionId] ?? 0;
	const effectiveSeed = seed ?? persistedSeed;
	if (s) {
		if (s.status === 'running') return 'running';
		if (s.status === 'awaiting_approval') return 'awaiting';
		// idle → seen/unseen by seq compare.
		const latest = Math.max(Number(s.latest_seq) || 0, effectiveSeed);
		return latest > (acked[sessionId] ?? 0) ? 'unseen' : 'seen';
	}
	// No live status this run: fall back to the persisted seed.
	return effectiveSeed > (acked[sessionId] ?? 0) ? 'unseen' : 'seen';
}

/** The latest committed `seq` seen for a session on the live status stream (0 if
 *  none yet). Reactive — a component reading this in an `$effect`/`$derived`
 *  re-runs when the session's status advances. The session list uses it as an
 *  activity signal to refresh a row's summary (and hence its sort position)
 *  without a page reload. */
export function latestSeqOf(sessionId: string): number {
	return latestSeqs[sessionId] ?? 0;
}

