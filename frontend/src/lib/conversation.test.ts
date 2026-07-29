import { describe, it, expect } from 'vitest';
import {
	apply,
	applyBatch,
	emptyState,
	pushOptimisticUser,
	stateFromView,
	type ConversationState
} from './conversation';
import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { SessionView } from '$lib/types/SessionView';

function fold(events: GatewayEvent[]): ConversationState {
	return events.reduce(apply, emptyState());
}

/** Build a RequestStarted committed event. `model` defaults to 'm'. */
function reqStarted(seq: number, model = 'm'): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Model', id: 'm' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Model: {
				RequestStarted: {
					request_id: `r${seq}`,
					provider: 'p',
					model,
					temperature: 0,
					max_tokens: null,
					tool_schemas_count: 0,
					input_tokens_estimate: 0
				}
			}
		}
	} as unknown as GatewayEvent;
}

/** Build a ContentBlock committed event. */
function contentBlock(
	seq: number,
	content:
		| { Text: { text: string } }
		| { Reasoning: { text: string } }
		| { ToolCall: { id: string; name: string; arguments: string } },
	index = 0
): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Model', id: 'm' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Model: {
				ContentBlock: { request_id: 'r', index, content }
			}
		}
	} as unknown as GatewayEvent;
}

function turnStarted(seq: number, input: string): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'ominiforge' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Turn: {
				Started: { turn_id: 't1', input }
			}
		}
	} as unknown as GatewayEvent;
}

/** Build a committed Turn lifecycle event (Completed/Interrupted) — the kind
 *  that survives history replay, unlike the live-only `turn_settled`. */
function turnLifecycle(seq: number, payload: Record<string, unknown>): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'ominiforge' },
		parent_event_id: null,
		turn_id: null,
		payload: { Turn: payload }
	} as unknown as GatewayEvent;
}

function turnCompleted(seq: number): GatewayEvent {
	return turnLifecycle(seq, { Completed: { turn_id: 't1' } });
}

function turnInterrupted(seq: number): GatewayEvent {
	return turnLifecycle(seq, {
		Interrupted: { turn_id: 't1', interrupted_at_event_id: { session_id: 's', seq } }
	});
}

describe('conversation fold', () => {
	// ── Optimistic user echo (send accepted, stream behind/dead) ────────

	it('optimistic bubble is replaced, not duplicated, when Turn.Started folds', () => {
		// The UX contract behind the dead-stream fix: the user sees their
		// message immediately after the 202, and when the authoritative event
		// finally arrives (live or via a reconnect's replay) exactly ONE bubble
		// remains — the committed one, carrying its seq.
		let state = pushOptimisticUser(emptyState(), 'hello');
		expect(state.items).toHaveLength(1);
		expect(state.items[0]).toMatchObject({ kind: 'user', text: 'hello', pending: true });

		state = apply(state, turnStarted(7, 'hello'));
		expect(state.items).toHaveLength(1);
		expect(state.items[0]).toMatchObject({ kind: 'user', text: 'hello', seq: 7 });
		expect(state.items[0]).not.toHaveProperty('pending');
	});

	it('Turn.Started only replaces a pending bubble with the SAME text', () => {
		// A queued/older optimistic echo whose text differs must survive an
		// unrelated turn start — otherwise one reconnect could eat a message
		// that is still awaiting its own event.
		let state = pushOptimisticUser(emptyState(), 'first');
		state = pushOptimisticUser(state, 'second');
		state = apply(state, turnStarted(3, 'first'));
		expect(state.items).toHaveLength(2);
		expect(state.items[0]).toMatchObject({ kind: 'user', text: 'second', pending: true });
		expect(state.items[1]).toMatchObject({ kind: 'user', text: 'first', seq: 3 });
	});

	// ── Dedup by seq (cold-path overlap) ────────────────────────────────

	it('dedup: a committed event with seq <= lastSeq is folded only once', () => {
		// The cold path folds fetched history (up to lastSeq) then resumes live via
		// SSE; the gateway subscribes to the live broadcast BEFORE reading its
		// replay log, so an event committed in the gap arrives twice — once in the
		// replay, once live. The fold must apply it once, not duplicate the item.
		const events: GatewayEvent[] = [
			turnStarted(1, 'hi'),
			turnStarted(1, 'hi'), // the duplicated gap event (same seq)
			turnStarted(2, 'there') // a genuinely new event still folds
		];

		const state = fold(events);
		const users = state.items.filter((i) => i.kind === 'user');
		expect(users).toHaveLength(2);
		expect(state.lastSeq).toBe(2);
	});

	// ── Streaming: temporal ordering ───────────────────────────────────

	it('streaming: reasoning appears before text when provider opens text block first', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: '' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 1, text: 'The user wants me' },
			{ type: 'delta', delta: 'reasoning', index: 1, text: ' to say hi' },
			{ type: 'delta', delta: 'text', index: 0, text: 'Hi there' },
			{ type: 'delta', delta: 'text', index: 0, text: ', friend! 👋' }
		];

		const items = fold(events).items;
		const text = items.filter((i) => i.kind === 'text');
		const reasoning = items.filter((i) => i.kind === 'reasoning');

		expect(text).toHaveLength(1);
		expect(reasoning).toHaveLength(1);
		expect(text[0].kind === 'text' && text[0].text).toBe('Hi there, friend! 👋');
		expect(reasoning[0].kind === 'reasoning' && reasoning[0].text).toBe(
			'The user wants me to say hi'
		);

		const reasoningIdx = items.findIndex((i) => i.kind === 'reasoning');
		const textIdx = items.findIndex((i) => i.kind === 'text');
		expect(reasoningIdx).toBeLessThan(textIdx);
	});

	it('streaming: normal order (reasoning first, text second) is preserved', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'thinking...' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'answer' }
		];

		const items = fold(events).items;
		expect(items[0].kind).toBe('reasoning');
		expect(items[1].kind).toBe('text');
	});

	it('streaming: empty text block is not created until content arrives', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: '' },
			{ type: 'delta', delta: 'text', index: 0, text: '' }
		];

		const items = fold(events).items;
		expect(items).toHaveLength(0);
	});

	it('streaming: block_start closes previous streaming item of same kind', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: 'first' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'second' }
		];

		const items = fold(events).items;
		const textItems = items.filter((i) => i.kind === 'text');
		expect(textItems).toHaveLength(2);
		expect(textItems[0].kind === 'text' && textItems[0].streaming).toBe(false);
		expect(textItems[0].kind === 'text' && textItems[0].text).toBe('first');
		expect(textItems[1].kind === 'text' && textItems[1].streaming).toBe(true);
		expect(textItems[1].kind === 'text' && textItems[1].text).toBe('second');
	});

	it('streaming: subsequent deltas extend the correct item via open map', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'a' },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'b' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'c' },
			{ type: 'delta', delta: 'text', index: 1, text: 'd' }
		];

		const items = fold(events).items;
		expect(items).toHaveLength(2);
		expect(items[0].kind === 'reasoning' && items[0].text).toBe('ab');
		expect(items[1].kind === 'text' && items[1].text).toBe('cd');
	});

	// ── Streaming: tool calls keep index-based tracking ────────────────

	it('streaming: tool args extend by index, not temporal order', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'tool_call', tool: 'shell' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: '{"cmd' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: '":"ls"}' }
		];

		const items = fold(events).items;
		expect(items).toHaveLength(1);
		expect(items[0].kind === 'tool' && items[0].args).toBe('{"cmd":"ls"}');
	});

	// ── Committed events ───────────────────────────────────────────────

	it('committed ContentBlock replaces the streaming preview, not appends', () => {
		const events: GatewayEvent[] = [
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: 'Hi th' },
			contentBlock(2, { Text: { text: 'Hi there' } })
		];

		const text = fold(events).items.filter((i) => i.kind === 'text');
		expect(text).toHaveLength(1);
		expect(text[0].kind === 'text' && text[0].streaming).toBe(false);
		expect(text[0].kind === 'text' && text[0].text).toBe('Hi there');
	});

	it('committed events put reasoning before text even when collector emits text first', () => {
		const events: GatewayEvent[] = [
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: '' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 1, text: 'thinking...' },
			{ type: 'delta', delta: 'text', index: 0, text: 'answer' },
			contentBlock(10, { Text: { text: 'answer' } }),
			contentBlock(11, { Reasoning: { text: 'thinking...' } })
		];

		const items = fold(events).items;
		const reasoningIdx = items.findIndex((i) => i.kind === 'reasoning');
		const textIdx = items.findIndex((i) => i.kind === 'text');
		expect(reasoningIdx).toBeGreaterThanOrEqual(0);
		expect(textIdx).toBeGreaterThanOrEqual(0);
		expect(reasoningIdx).toBeLessThan(textIdx);
	});

	it('committed events: normal order (reasoning first) stays correct', () => {
		const events: GatewayEvent[] = [
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'thinking...' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'answer' },
			contentBlock(10, { Reasoning: { text: 'thinking...' } }),
			contentBlock(11, { Text: { text: 'answer' } })
		];

		const items = fold(events).items;
		const reasoningIdx = items.findIndex((i) => i.kind === 'reasoning');
		const textIdx = items.findIndex((i) => i.kind === 'text');
		expect(reasoningIdx).toBeLessThan(textIdx);
	});

	// ── User message visibility ────────────────────────────────────────

	it('user message survives committed event truncation', () => {
		// Normal flow: Turn.Started → RequestStarted → deltas → ContentBlocks.
		// The user message must survive the commitBlock truncation.
		const events: GatewayEvent[] = [
			turnStarted(1, 'hello'),
			reqStarted(2),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'think...' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'answer' },
			contentBlock(3, { Reasoning: { text: 'think...' } }),
			contentBlock(4, { Text: { text: 'answer' } })
		];

		const items = fold(events).items;
		const user = items.filter((i) => i.kind === 'user');
		expect(user).toHaveLength(1);
		expect(user[0].kind === 'user' && user[0].text).toBe('hello');
		// User message should be first
		expect(items[0].kind).toBe('user');
	});

	it('user item carries the Turn.Started seq as its fork point', () => {
		// The seq is what a "fork from this turn" affordance passes as `at_seq` to
		// POST /sessions/{id}/fork. Without it the UI can't branch at a specific
		// user turn, so this asserts the fold threads the committed event's seq
		// through to the item — not merely that a user item exists.
		const items = fold([turnStarted(7, 'branch here')]).items;
		const user = items.find((i) => i.kind === 'user');
		expect(user?.kind === 'user' && user.seq).toBe(7);
	});

	// ── Race condition: deltas before RequestStarted ───────────────────

	it('no duplication when deltas arrive before RequestStarted', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'think...' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'answer' },
			reqStarted(1),
			contentBlock(2, { Reasoning: { text: 'think...' } }),
			contentBlock(3, { Text: { text: 'answer' } })
		];

		const items = fold(events).items;
		const reasoning = items.filter((i) => i.kind === 'reasoning');
		const text = items.filter((i) => i.kind === 'text');

		expect(reasoning).toHaveLength(1);
		expect(text).toHaveLength(1);
		expect(reasoning[0].kind === 'reasoning' && reasoning[0].text).toBe('think...');
		expect(text[0].kind === 'text' && text[0].text).toBe('answer');
		expect(items.findIndex((i) => i.kind === 'reasoning')).toBeLessThan(
			items.findIndex((i) => i.kind === 'text')
		);
	});

	it('no duplication across multiple rounds', () => {
		const events: GatewayEvent[] = [
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'tool_call', tool: 'read' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: '{"path":"f.txt"}' },
			contentBlock(2, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"f.txt"}' } }),

			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'hmm' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'result' },
			reqStarted(3),
			contentBlock(4, { Reasoning: { text: 'hmm' } }),
			contentBlock(5, { Text: { text: 'result' } })
		];

		const items = fold(events).items;
		const reasoning = items.filter((i) => i.kind === 'reasoning');
		const text = items.filter((i) => i.kind === 'text');
		expect(reasoning).toHaveLength(1);
		expect(text).toHaveLength(1);
		expect(items[0].kind).toBe('tool');
		expect(items[1].kind).toBe('reasoning');
		expect(items[2].kind).toBe('text');
	});

	// ── Request lifecycle ──────────────────────────────────────────────

	it('a new model request resets block indices', () => {
		const events: GatewayEvent[] = [
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: 'first' },
			reqStarted(2),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: 'second' }
		];

		const text = fold(events).items.filter((i) => i.kind === 'text');
		expect(text).toHaveLength(2);
		expect(text.map((t) => (t.kind === 'text' ? t.text : ''))).toEqual(['first', 'second']);
	});

	it('turn_settled clears commit state', () => {
		const events: GatewayEvent[] = [
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: 'answer' },
			contentBlock(2, { Text: { text: 'answer' } }),
			{ type: 'turn_settled', incomplete: null }
		];

		const state = fold(events);
		expect(state.requestStart).toBeUndefined();
		expect(state.requestCommitted).toBeUndefined();
		expect(state.commitBase).toBeUndefined();
	});

	// ── Race condition: turn_settled before ContentBlock events ────────
	//
	// This reproduces the duplication bug: the backend's event-forwarder task
	// runs on a separate tokio task. After a turn completes, TurnSettled is
	// sent from the turn task (synchronous) while ContentBlock events are
	// forwarded by the separate forwarder task. If the turn task doesn't yield
	// between collect_round.finish() and on_turn_done(), TurnSettled reaches
	// the frontend before ContentBlock events, clearing requestStart and
	// preventing commitBlock from truncating streaming previews.

	it('no duplication when turn_settled arrives before ContentBlock (normal order)', () => {
		const events: GatewayEvent[] = [
			turnStarted(1, 'hello'),
			reqStarted(2),
			// Streaming phase
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'think...' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'answer' },
			// turn_settled arrives BEFORE committed events (the race)
			{ type: 'turn_settled', incomplete: null },
			// Committed events arrive late
			contentBlock(3, { Reasoning: { text: 'think...' } }),
			contentBlock(4, { Text: { text: 'answer' } })
		];

		const items = fold(events).items;
		const reasoning = items.filter((i) => i.kind === 'reasoning');
		const text = items.filter((i) => i.kind === 'text');

		expect(reasoning).toHaveLength(1);
		expect(text).toHaveLength(1);
		expect(reasoning[0].kind === 'reasoning' && reasoning[0].text).toBe('think...');
		expect(text[0].kind === 'text' && text[0].text).toBe('answer');
		// Reasoning must come before text
		expect(items.findIndex((i) => i.kind === 'reasoning')).toBeLessThan(
			items.findIndex((i) => i.kind === 'text')
		);
		// User message preserved
		expect(items[0].kind).toBe('user');
	});

	it('no duplication when turn_settled arrives before ContentBlock (reversed commit order)', () => {
		// The collector may emit ContentBlock events in either order (text
		// first, or reasoning first). Both must work.
		const events: GatewayEvent[] = [
			turnStarted(1, 'hi'),
			reqStarted(2),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'hmm' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'hello' },
			{ type: 'turn_settled', incomplete: null },
			// Text committed before reasoning
			contentBlock(3, { Text: { text: 'hello' } }),
			contentBlock(4, { Reasoning: { text: 'hmm' } })
		];

		const items = fold(events).items;
		const reasoning = items.filter((i) => i.kind === 'reasoning');
		const text = items.filter((i) => i.kind === 'text');

		expect(reasoning).toHaveLength(1);
		expect(text).toHaveLength(1);
		// Reasoning should still come before text (commitBase reorders)
		expect(items.findIndex((i) => i.kind === 'reasoning')).toBeLessThan(
			items.findIndex((i) => i.kind === 'text')
		);
	});

	it('no duplication when turn_settled arrives before ContentBlock across multi-round turn', () => {
		// Multi-round: round 1 (tool call) commits normally, then round 2
		// has the race condition.
		const events: GatewayEvent[] = [
			turnStarted(1, 'do something'),
			reqStarted(2),
			// Round 1: tool call (committed normally before turn_settled)
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'tool_call', tool: 'read' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: '{"path":"f.txt"}' },
			contentBlock(3, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"f.txt"}' } }),
			// Round 2: reasoning + text with the race
			reqStarted(4),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 0, text: 'analyzing...' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 1, text: 'here is the result' },
			// turn_settled arrives before round 2's committed events
			{ type: 'turn_settled', incomplete: null },
			contentBlock(5, { Reasoning: { text: 'analyzing...' } }),
			contentBlock(6, { Text: { text: 'here is the result' } })
		];

		const items = fold(events).items;
		const reasoning = items.filter((i) => i.kind === 'reasoning');
		const text = items.filter((i) => i.kind === 'text');

		expect(reasoning).toHaveLength(1);
		expect(text).toHaveLength(1);
		expect(reasoning[0].kind === 'reasoning' && reasoning[0].text).toBe('analyzing...');
		expect(text[0].kind === 'text' && text[0].text).toBe('here is the result');
		// Tool call from round 1 preserved
		expect(items[0].kind).toBe('user');
		expect(items[1].kind).toBe('tool');
		expect(items[2].kind).toBe('reasoning');
		expect(items[3].kind).toBe('text');
	});

	// ── Runtime-layer model capture (B4) ───────────────────────────────
	//
	// The fold records every distinct model a RequestStarted used, so the UI can
	// validate the runtime layer against the configured model and fail loud on
	// divergence (a subagent/fork on a different model). The config layer remains
	// the display source; this set is the validation source only.

	it('runtime models: a single request records its model', () => {
		const state = fold([reqStarted(1, 'sonnet')]);
		expect([...state.runtimeModels]).toEqual(['sonnet']);
	});

	it('runtime models: repeated use of the same model is deduplicated', () => {
		// Same model across rounds must not produce duplicates — the set is what
		// the divergence check compares against, so duplicates would be noise.
		const state = fold([reqStarted(1, 'sonnet'), reqStarted(2, 'sonnet'), reqStarted(3, 'sonnet')]);
		expect([...state.runtimeModels]).toEqual(['sonnet']);
	});

	it('runtime models: distinct models are all captured (divergence is detectable)', () => {
		// A subagent switching to haiku mid-session is exactly the case B4 must
		// surface: both models present means the UI can flag haiku ≠ configured.
		const state = fold([reqStarted(1, 'sonnet'), reqStarted(2, 'haiku'), reqStarted(3, 'sonnet')]);
		expect([...state.runtimeModels].sort()).toEqual(['haiku', 'sonnet']);
	});

	it('runtime models: empty before any request', () => {
		expect(emptyState().runtimeModels.size).toBe(0);
	});

	// ── Turn running flag (drives Cancel visibility) ───────────────────
	//
	// Cancel is meaningful only while a turn runs (the gateway ignores Cancel
	// when idle), so the button's visibility hangs entirely on turnRunning.
	// These pin the property that matters: it tracks the lifecycle AND lands
	// correct on history replay, where only committed events exist.

	it('turnRunning: false before any turn', () => {
		expect(emptyState().turnRunning).not.toBe(true);
		expect(fold([reqStarted(1)]).turnRunning).not.toBe(true);
	});

	it('turnRunning: true after Turn.Started, while the turn is live', () => {
		const state = fold([turnStarted(1, 'hi'), reqStarted(2)]);
		expect(state.turnRunning).toBe(true);
	});

	it('turnRunning: live turn_settled ends the turn', () => {
		// The live path: a turn completes in-session and the gateway emits the
		// ephemeral turn_settled. Cancel must disappear the instant it lands.
		const state = fold([
			turnStarted(1, 'hi'),
			reqStarted(2),
			contentBlock(3, { Text: { text: 'done' } }),
			{ type: 'turn_settled', incomplete: null }
		]);
		expect(state.turnRunning).toBe(false);
	});

	it('turnRunning: reconstructs as false on history replay via committed Completed', () => {
		// Replaying a finished session sends committed events only — never the
		// live turn_settled. Without folding the committed Turn.Completed the flag
		// would stay stuck true and show a stale Cancel on every loaded session.
		// This is the test that would fail if we relied on turn_settled alone.
		const replay = fold([
			turnStarted(1, 'hi'),
			reqStarted(2),
			contentBlock(3, { Text: { text: 'done' } }),
			turnCompleted(4)
		]);
		expect(replay.turnRunning).toBe(false);
	});

	it('turnRunning: committed Interrupted ends the turn', () => {
		const state = fold([turnStarted(1, 'hi'), reqStarted(2), turnInterrupted(3)]);
		expect(state.turnRunning).toBe(false);
	});

	it('turnRunning: a notice (e.g. "turn cancelled") ends the turn', () => {
		// Cancel aborts the task and emits a notice rather than a committed
		// terminator; the notice must also drop the flag so the live session's
		// Cancel button clears right after the user cancels.
		const state = fold([
			turnStarted(1, 'hi'),
			reqStarted(2),
			{ type: 'notice', message: 'turn cancelled' }
		]);
		expect(state.turnRunning).toBe(false);
	});

	it('turnRunning: a second turn re-arms the flag after the first settles', () => {
		const state = fold([
			turnStarted(1, 'one'),
			reqStarted(2),
			turnCompleted(3),
			turnStarted(4, 'two'),
			reqStarted(5)
		]);
		expect(state.turnRunning).toBe(true);
	});
});

// ── Todo control tool: folded into structured todo cards ───────────────
//
// `todo` is a control tool (src/agent/todo.rs): its calls arrive as committed
// ToolCall ContentBlocks whose `arguments` is a TodoOp JSON. The fold must turn
// these into a single structured `todo` item (one card per `init`, later ops
// mutating it in place) — NOT a generic tool block per op.

/** A committed `todo` tool call carrying the given op JSON as its arguments. */
function todoCall(seq: number, op: object): GatewayEvent {
	return contentBlock(seq, {
		ToolCall: { id: `p${seq}`, name: 'todo', arguments: JSON.stringify(op) }
	});
}

/** Pull the single todo card's steps (asserts exactly one exists). */
function todoSteps(state: ConversationState) {
	const todos = state.items.filter((i) => i.kind === 'todo');
	expect(todos).toHaveLength(1);
	const p = todos[0];
	return p.kind === 'todo' ? p.steps : [];
}

describe('todo fold', () => {
	it('init creates one todo card with sequential pending steps (no tool blocks)', () => {
		const state = fold([
			reqStarted(1),
			todoCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }, { content: 'c' }] })
		]);
		// A todo call must never surface as a generic tool block.
		expect(state.items.filter((i) => i.kind === 'tool')).toHaveLength(0);
		const steps = todoSteps(state);
		expect(steps.map((s) => s.id)).toEqual(['1', '2', '3']);
		expect(steps.every((s) => s.status === 'pending')).toBe(true);
	});

	it('start/complete mutate the existing card in place, not append new cards', () => {
		const state = fold([
			reqStarted(1),
			todoCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }] }),
			reqStarted(3),
			todoCall(4, { ops: [{ op: 'start', id: '1' }] }),
			reqStarted(5),
			todoCall(6, { ops: [{ op: 'complete', id: '1' }] })
		]);
		const steps = todoSteps(state);
		expect(steps[0].status).toBe('completed');
		expect(steps[1].status).toBe('pending');
	});

	it('one call carrying several ops applies them all in order', () => {
		const state = fold([
			reqStarted(1),
			todoCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }, { content: 'c' }] }),
			reqStarted(3),
			todoCall(4, {
				ops: [
					{ op: 'start', id: '1' },
					{ op: 'complete', id: '1' },
					{ op: 'start', id: '2' }
				]
			})
		]);
		const steps = todoSteps(state);
		expect(steps.map((s) => s.status)).toEqual(['completed', 'in_progress', 'pending']);
	});

	it('cancel and block record their reason', () => {
		const state = fold([
			reqStarted(1),
			todoCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }] }),
			reqStarted(3),
			todoCall(4, { ops: [{ op: 'cancel', id: '1', reason: 'no such tool' }] }),
			reqStarted(5),
			todoCall(6, { ops: [{ op: 'block', id: '2', reason: 'needs API key' }] })
		]);
		const steps = todoSteps(state);
		expect(steps[0].status).toBe('cancelled');
		expect(steps[0].reason).toBe('no such tool');
		expect(steps[1].status).toBe('blocked');
		expect(steps[1].reason).toBe('needs API key');
	});

	it('add appends at end and inserts after an anchor with max+1 id', () => {
		const state = fold([
			reqStarted(1),
			todoCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }] }),
			reqStarted(3),
			todoCall(4, { ops: [{ op: 'add', content: 'end' }] }),
			reqStarted(5),
			todoCall(6, { ops: [{ op: 'add', content: 'mid', after_id: '1' }] })
		]);
		const steps = todoSteps(state);
		expect(steps.map((s) => s.content)).toEqual(['a', 'mid', 'b', 'end']);
		// id is max+1, unaffected by insert position
		const mid = steps.find((s) => s.content === 'mid');
		expect(mid?.id).toBe('4');
	});

	it('a non-init op with no prior todo list is a benign no-op', () => {
		const state = fold([reqStarted(1), todoCall(2, { ops: [{ op: 'start', id: '1' }] })]);
		expect(state.items.filter((i) => i.kind === 'todo')).toHaveLength(0);
		expect(state.items.filter((i) => i.kind === 'tool')).toHaveLength(0);
	});

	it('start on an unknown id leaves the todo list unchanged', () => {
		const state = fold([
			reqStarted(1),
			todoCall(2, { op: 'init', steps: [{ content: 'a' }] }),
			reqStarted(3),
			todoCall(4, { ops: [{ op: 'start', id: '99' }] })
		]);
		expect(todoSteps(state)[0].status).toBe('pending');
	});

	it('a second init starts a fresh card, preserving the first as history', () => {
		const state = fold([
			reqStarted(1),
			todoCall(2, { op: 'init', steps: [{ content: 'a' }] }),
			reqStarted(3),
			todoCall(4, { ops: [{ op: 'complete', id: '1' }] }),
			reqStarted(5),
			todoCall(6, { op: 'init', steps: [{ content: 'x' }, { content: 'y' }] })
		]);
		const todos = state.items.filter((i) => i.kind === 'todo');
		expect(todos).toHaveLength(2);
		expect(todos[0].kind === 'todo' && todos[0].steps[0].status).toBe('completed');
		expect(todos[1].kind === 'todo' && todos[1].steps.map((s) => s.content)).toEqual(['x', 'y']);
	});

	it('streaming todo placeholder is replaced by the committed card (no duplication)', () => {
		const state = fold([
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'tool_call', tool: 'todo' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: '{"op":"in' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: 'it","steps":[{"content":"a"}]}' },
			todoCall(2, { op: 'init', steps: [{ content: 'a' }] })
		]);
		const todos = state.items.filter((i) => i.kind === 'todo');
		expect(todos).toHaveLength(1);
		expect(todos[0].kind === 'todo' && todos[0].streaming).toBe(false);
		expect(todos[0].kind === 'todo' && todos[0].steps[0].content).toBe('a');
	});

	it('a later turn op mutates a todo card from an earlier request', () => {
		// The card lives before the second RequestStarted; commitBlock truncation
		// must not lose it, and the op must still target it.
		const state = fold([
			turnStarted(1, 'go'),
			reqStarted(2),
			todoCall(3, { op: 'init', steps: [{ content: 'a' }] }),
			turnCompleted(4),
			turnStarted(5, 'continue'),
			reqStarted(6),
			todoCall(7, { ops: [{ op: 'complete', id: '1' }] })
		]);
		const steps = todoSteps(state);
		expect(steps[0].status).toBe('completed');
	});
});

function permRequested(seq: number, callId: string, tool: string, preview?: string): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'runtime' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Permission: {
				Requested: { call_id: callId, tool_name: tool, input: { path: 'x.txt' }, preview }
			}
		}
	} as unknown as GatewayEvent;
}

function permDecided(
	seq: number,
	callId: string,
	outcome: 'Approved' | 'Rejected' | 'AutoDenied'
): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'runtime' },
		parent_event_id: null,
		turn_id: null,
		payload: { Permission: { Decided: { call_id: callId, outcome, decided_by: 'user' } } }
	} as unknown as GatewayEvent;
}

/** A `Tool::Failed` event pairing back to the ToolCall content block at
 *  `callSeq` (how the fold finds the card). `message` carries the denial code
 *  (e.g. `denied_by_user`) as the fold only reads `error.message`. */
function toolFailed(seq: number, callSeq: number, message: string): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'runtime' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Tool: {
				Failed: {
					tool_call_event_id: { session_id: 's', seq: callSeq },
					error: { message }
				}
			}
		}
	} as unknown as GatewayEvent;
}

/** A `Tool::Completed` event pairing back to the ToolCall content block at
 *  `callSeq`, carrying `text` as its result. */
function toolCompleted(seq: number, callSeq: number, text: string): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'runtime' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Tool: {
				Completed: {
					tool_call_event_id: { session_id: 's', seq: callSeq },
					result: { content: [{ Text: text }], is_error: false, error_code: null }
				}
			}
		}
	} as unknown as GatewayEvent;
}

/** A `Tool::Completed` whose result carries a `TextView` (audience "ui") block
 *  after its primary text — the shape `edit`/`write` produce with a diff view
 *  (`doc/tool-view.md`). */
function toolCompletedView(seq: number, callSeq: number, text: string, view: string): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'runtime' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Tool: {
				Completed: {
					tool_call_event_id: { session_id: 's', seq: callSeq },
					result: {
						content: [{ Text: text }, { TextView: { text: view, audience: 'ui' } }],
						is_error: false,
						error_code: null
					}
				}
			}
		}
	} as unknown as GatewayEvent;
}

describe('permission approval fold', () => {
	// The approval prompt attaches to the gated call's own tool card (folded from
	// the earlier ToolCall content block, bridged by the model-assigned call id) —
	// there is no separate approval item kind.
	it('Requested marks the matching tool card approval-pending', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"x.txt"}' } }),
			permRequested(2, 'c1', 'write')
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		expect(tools).toHaveLength(1);
		expect(tools[0].kind === 'tool' && tools[0].approvalPending).toBe(true);
		expect(tools[0].kind === 'tool' && tools[0].callId).toBe('c1');
	});

	it('Requested with no matching tool card synthesizes one (defensive)', () => {
		// The ContentBlock always commits first in practice; if it is somehow
		// missing, the prompt must still surface instead of vanishing.
		const state = fold([permRequested(1, 'c9', 'write')]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		expect(tools).toHaveLength(1);
		const t = tools[0];
		expect(t.kind === 'tool' && t.approvalPending).toBe(true);
		expect(t.kind === 'tool' && t.name).toBe('write');
		expect(t.kind === 'tool' && t.args).toContain('x.txt');
	});

	it('Decided clears the pending flag; the card stays for its tool outcome', () => {
		// Approved or rejected, the card's final status arrives via the paired
		// Tool::Completed/Failed — the Decided itself only disarms the prompt.
		for (const outcome of ['Approved', 'Rejected', 'AutoDenied'] as const) {
			const state = fold([
				contentBlock(1, { ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"x.txt"}' } }),
				permRequested(2, 'c1', 'write'),
				permDecided(3, 'c1', outcome)
			]);
			const tools = state.items.filter((i) => i.kind === 'tool');
			expect(tools).toHaveLength(1);
			expect(tools[0].kind === 'tool' && tools[0].approvalPending).toBe(false);
		}
	});

	it('a rejection then lands as the tool card error via Tool::Failed', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'shell', arguments: '{"command":"curl x"}' } }),
			permRequested(2, 'c1', 'shell'),
			permDecided(3, 'c1', 'Rejected'),
			toolFailed(4, 1, 'denied_by_user')
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.status).toBe('error');
		expect(t?.kind === 'tool' && t.result).toContain('denied_by_user');
	});

	it('Requested carries the would-be diff preview onto the gated card', () => {
		// The gate computed the diff for edit/write; it lands on the card so the
		// human approves the actual change, not abstract args (doc/permission.md §6).
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'edit', arguments: '{"edits":[]}' } }),
			permRequested(2, 'c1', 'edit', '--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-x\n+y')
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.approvalPending).toBe(true);
		expect(t?.kind === 'tool' && t.preview).toContain('@@ -1,1 +1,1 @@');
	});

	it('the executed view replaces the preview; a no-view result keeps the preview', () => {
		// Approved call runs: the executed `view` (TextView) takes over. When the
		// call produced no view (e.g. a no-op edit), the preview stays so the card
		// never flashes empty between approve and settle.
		const withView = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'edit', arguments: '{}' } }),
			permRequested(2, 'c1', 'edit', 'PREVIEW'),
			permDecided(3, 'c1', 'Approved'),
			toolCompletedView(4, 1, 'edited f (1 replacement)', 'FINAL')
		]);
		const t1 = withView.items.find((i) => i.kind === 'tool');
		expect(t1?.kind === 'tool' && t1.view).toBe('FINAL');

		const noView = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'edit', arguments: '{}' } }),
			permRequested(2, 'c1', 'edit', 'PREVIEW'),
			permDecided(3, 'c1', 'Approved'),
			toolCompleted(4, 1, 'edited f (no change)')
		]);
		const t2 = noView.items.find((i) => i.kind === 'tool');
		expect(t2?.kind === 'tool' && t2.view).toBe('PREVIEW');
	});

	it('a Decided for an unknown call id leaves the pending flag untouched', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"x.txt"}' } }),
			permRequested(2, 'c1', 'write'),
			permDecided(3, 'other', 'Approved')
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.approvalPending).toBe(true);
	});

	it('a turn Interrupted while an ask is pending disarms the zombie prompt', () => {
		// Cancel/crash mid-ask: the Decided will never commit, so the prompt can
		// never resolve. The Interrupted fold must clear the flag or a replayed
		// history shows a frozen approval prompt with dead buttons. The tool card
		// itself stays (it is real history).
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'shell', arguments: '{"command":"x"}' } }),
			permRequested(2, 'c1', 'shell'),
			turnInterrupted(3)
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.approvalPending).toBe(false);
	});

	// ── Out-of-order: Requested before its ToolCall ContentBlock ────────
	//
	// Event delivery can invert the commit order (the ask fires the moment the
	// gate suspends the call; the collector's ContentBlock forwards on another
	// task). The synthesized card must be COMPLETED by the late block — never
	// duplicated — and re-registered under the real ToolCall seq, or the paired
	// Tool::Completed/Failed (which keys on that seq) never finds the card and
	// it spins in `running` forever.

	it('Requested before its ToolCall ContentBlock: the late block completes the orphan', () => {
		const state = fold([
			permRequested(1, 'c1', 'write'),
			contentBlock(2, { ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"x.txt"}' } }),
			permDecided(3, 'c1', 'Approved'),
			toolCompleted(4, 2, 'wrote x.txt')
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		// One card, not two; the orphan was completed in place.
		expect(tools).toHaveLength(1);
		const t = tools[0];
		// Backfilled with the REAL ToolCall seq/args (the Requested seq is gone),
		// which is what let the Completed below pair.
		expect(t.kind === 'tool' && t.seq).toBe(2);
		expect(t.kind === 'tool' && t.args).toBe('{"path":"x.txt"}');
		expect(t.kind === 'tool' && t.approvalPending).toBe(false);
		expect(t.kind === 'tool' && t.status).toBe('done');
		expect(t.kind === 'tool' && t.result).toContain('wrote x.txt');
		expect(state.orphanTools.size).toBe(0);
	});

	it('Requested before ContentBlock after RequestStarted: the prompt survives truncation', () => {
		// Same reorder one request in: the first committed block truncates items
		// appended past requestStart — orphan included. The rebuilt card must
		// keep the outstanding ask (a deciding human must not lose the buttons)
		// and still pair its result.
		const state = fold([
			reqStarted(1),
			permRequested(2, 'c1', 'write'),
			contentBlock(3, { ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"x.txt"}' } }),
			toolCompleted(4, 3, 'wrote x.txt')
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		expect(tools).toHaveLength(1);
		const t = tools[0];
		expect(t.kind === 'tool' && t.seq).toBe(3);
		expect(t.kind === 'tool' && t.approvalPending).toBe(true);
		expect(t.kind === 'tool' && t.status).toBe('done');
		expect(t.kind === 'tool' && t.result).toContain('wrote x.txt');
		expect(state.orphanTools.size).toBe(0);
	});
});

// ── UI view fold: `Content::TextView` lands on the tool card, never in result ──
//
// The backend attaches a `TextView` (audience "ui") block carrying the precise
// diff/full-content view for `edit`/`write` (`doc/tool-view.md`). It must reach
// the card's `view` verbatim and NEVER leak into the model-facing `result`.

describe('tool view fold', () => {
	it('a TextView block lands on the card as view, kept out of result', () => {
		const state = fold([
			contentBlock(1, {
				ToolCall: {
					id: 'c1',
					name: 'edit',
					arguments: '{"edits":[{"path":"a.txt","old":["b"],"new":["B"]}]}'
				}
			}),
			toolCompletedView(
				2,
				1,
				'edited a.txt (1 replacement)',
				'--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c'
			)
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.result).toBe('edited a.txt (1 replacement)');
		expect(t?.kind === 'tool' && t.view).toContain('@@');
		// The diff view must NOT leak into the model-facing result.
		expect(t?.kind === 'tool' && t.result).not.toContain('@@');
	});

	it('a TextView with a non-"ui" audience is ignored', () => {
		const ev = {
			type: 'event',
			schema_version: 'ominiforge.event.v1',
			seq: 2,
			session_id: 's',
			timestamp: '2026-06-24T00:00:00Z',
			source: { kind: 'Runtime', id: 'runtime' },
			parent_event_id: null,
			turn_id: null,
			payload: {
				Tool: {
					Completed: {
						tool_call_event_id: { session_id: 's', seq: 1 },
						result: {
							content: [
								{ Text: 'edited a.txt (1 replacement)' },
								{ TextView: { text: 'tui-only', audience: 'tui' } }
							],
							is_error: false,
							error_code: null
						}
					}
				}
			}
		} as unknown as GatewayEvent;
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'edit', arguments: '{}' } }),
			ev
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.view).toBeUndefined();
	});

	it('a tool with no TextView block has view undefined', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"a.txt"}' } }),
			toolCompleted(2, 1, '[a.txt]\n1:hello')
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.view).toBeUndefined();
	});
});

/** A `Tool::Completed` whose result carries MULTIPLE content blocks — the shape
 *  a tool produces when it appends supplementary content (LSP diagnostics) after
 *  its primary result. */
function toolCompletedMulti(seq: number, callSeq: number, texts: string[]): GatewayEvent {
	return {
		type: 'event',
		schema_version: 'ominiforge.event.v1',
		seq,
		session_id: 's',
		timestamp: '2026-06-24T00:00:00Z',
		source: { kind: 'Runtime', id: 'runtime' },
		parent_event_id: null,
		turn_id: null,
		payload: {
			Tool: {
				Completed: {
					tool_call_event_id: { session_id: 's', seq: callSeq },
					result: {
						content: texts.map((t) => ({ Text: t })),
						is_error: false,
						error_code: null
					}
				}
			}
		}
	} as unknown as GatewayEvent;
}

describe('tool diagnostics split (LSP assist)', () => {
	// The primary result (content[0]) stays `result`; anything appended after it
	// (LSP diagnostics) goes to `diagnostics` — kept out of the primary view so
	// it renders only in the debug fold, while still reaching the model via the
	// backend's content flattening. See doc/lsp.md §5.
	it('splits a diagnostics block off the primary result', () => {
		const state = fold([
			contentBlock(1, {
				ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"a.rs","content":"fn f(){}"}' }
			}),
			toolCompletedMulti(2, 1, [
				'wrote a.rs (new, 1 lines)',
				'\n[diagnostics: a.rs] 1 issue(s)\n  1:9 error: expected type'
			])
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.result).toBe('wrote a.rs (new, 1 lines)');
		expect(t?.kind === 'tool' && t.diagnostics).toContain('error: expected type');
		// The diagnostics text must NOT leak into `result` (primary view).
		expect(t?.kind === 'tool' && t.result).not.toContain('diagnostics');
	});

	// The common case — a single content block — leaves `diagnostics` undefined,
	// so nothing changes for tools without the LSP assist.
	it('a single-block result has no diagnostics field', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"a.txt"}' } }),
			toolCompleted(2, 1, '[a.txt]\n1:hello')
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.diagnostics).toBeUndefined();
	});

	// The split is gated on the tool NAME, not positional index: a non-assist
	// tool (e.g. an MCP tool returning text+something) that legitimately emits
	// multiple content entries must keep ALL of them in the primary result, not
	// silently relegate the tail to the debug fold. Regression guard for the
	// tool-agnostic-split bug.
	it('a non-assist tool with multiple content blocks keeps them all in result', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'search', arguments: '{"q":"x"}' } }),
			toolCompletedMulti(2, 1, ['hit one', 'hit two'])
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.result).toBe('hit onehit two');
		expect(t?.kind === 'tool' && t.diagnostics).toBeUndefined();
	});
});

// ── Stable item ids + batch folding ─────────────────────────────────
//
// The conversation renders through a keyed each on `Item.id`, and the session
// page folds the replay burst via `applyBatch`. These pin the two properties
// the rendering layer relies on: ids are unique/stable across folds, and a
// batched fold is indistinguishable from an event-at-a-time fold.

describe('item identity + batch fold', () => {
	it('every folded item carries a unique id; committed items key on their event seq', () => {
		const events: GatewayEvent[] = [
			turnStarted(1, 'hello'),
			reqStarted(2),
			contentBlock(3, { Reasoning: { text: 'think' } }),
			contentBlock(4, { Text: { text: 'answer' } }),
			contentBlock(5, { ToolCall: { id: 'c1', name: 'read', arguments: '{}' } }),
			toolCompleted(6, 5, 'done'),
			turnCompleted(7),
			{ type: 'notice', message: 'note' }
		];
		const state = fold(events);
		const ids = state.items.map((i) => i.id);
		expect(new Set(ids).size).toBe(ids.length);
		// Committed items key on the producing event's seq.
		const user = state.items.find((i) => i.kind === 'user');
		expect(user?.id).toBe(1);
		const text = state.items.find((i) => i.kind === 'text');
		expect(text?.id).toBe(4);
		// Transient items (notices) draw from the negative namespace, never
		// colliding with a seq.
		const notice = state.items.find((i) => i.kind === 'notice');
		expect(notice !== undefined && notice.id < 0).toBe(true);
	});

	it('an orphan permission card is re-keyed from the Requested seq to the ToolCall seq', () => {
		// The synthesized orphan is keyed on the Requested event's seq; the late
		// ContentBlock completing it must re-key to the real ToolCall seq, or the
		// card's DOM identity carries a seq that pairs with nothing.
		const state = fold([
			permRequested(1, 'c1', 'write'),
			contentBlock(2, { ToolCall: { id: 'c1', name: 'write', arguments: '{}' } })
		]);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.id).toBe(2);
		expect(t?.kind === 'tool' && t.seq).toBe(2);
	});

	it('applyBatch folds identically to event-at-a-time apply', () => {
		// The replay burst folds through applyBatch; any divergence from the
		// per-event path would render history differently from live output.
		const events: GatewayEvent[] = [
			turnStarted(1, 'hello'),
			reqStarted(2),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: 'ans' },
			contentBlock(3, { Text: { text: 'ans' } }),
			contentBlock(4, { ToolCall: { id: 'c1', name: 'read', arguments: '{}' } }),
			toolCompleted(5, 4, 'done'),
			turnCompleted(6),
			turnStarted(7, 'next'),
			reqStarted(8),
			todoCall(9, { op: 'init', steps: [{ content: 'a' }] }),
			{ type: 'turn_settled', incomplete: null },
			{ type: 'notice', message: 'note' }
		];
		const batched = applyBatch(emptyState(), events);
		const incremental = fold(events);
		expect(batched).toEqual(incremental);
	});
});

describe('stateFromView (server-folded history)', () => {
	it('a running tool card in the view must pair with a Tool::Completed that lands after subscribe', () => {
		// Mid-turn session open: the ToolCall block committed BEFORE the view
		// fetch (so the card renders from the view, keyed by its seq ≤ last_seq),
		// but its result commits after. `pairResult` looks the call up in
		// `toolSeqs` — if stateFromView doesn't rebuild that entry, the result
		// is silently dropped and the card stays `running` until the session is
		// reopened. This is the bug a user actually sees: open a busy session,
		// one card never finishes.
		const view: SessionView = {
			items: [
				{ kind: 'user', id: 1, seq: 1, text: 'go' },
				{
					kind: 'tool',
					id: 2,
					seq: 2,
					callId: 'c1',
					name: 'read',
					args: '{}',
					status: 'running'
				}
			],
			last_seq: 2,
			turn_running: true,
			runtime_models: ['m']
		};
		const state = apply(stateFromView(view), toolCompleted(3, 2, 'done'));
		const card = state.items.find((i) => i.kind === 'tool');
		expect(card?.kind === 'tool' && card.status).toBe('done');
		expect(card?.kind === 'tool' && card.result).toBe('done');
	});

	it('a live ContentBlock after the view must append, not truncate the view items', () => {
		// Mid-turn open: the view already holds committed blocks from the
		// in-flight request. `requestCommitted` must be true so the first live
		// ContentBlock appends instead of mis-firing the first-commit truncation
		// that replaces streaming previews (there are none in a view — the slice
		// would eat real history).
		const view: SessionView = {
			items: [
				{ kind: 'user', id: 1, seq: 1, text: 'go' },
				{ kind: 'text', id: 2, text: 'partial answer', streaming: false }
			],
			last_seq: 2,
			turn_running: true,
			runtime_models: ['m']
		};
		const state = apply(
			stateFromView(view),
			contentBlock(3, { Text: { text: ' rest of answer' } })
		);
		expect(state.items).toHaveLength(3);
		expect(state.items[0]).toMatchObject({ kind: 'user', text: 'go' });
		expect(state.items[1]).toMatchObject({ kind: 'text', text: 'partial answer' });
		expect(state.items[2]).toMatchObject({ kind: 'text', text: ' rest of answer' });
	});

	it('a new RequestStarted after the view starts fresh bookkeeping past the view', () => {
		// The next request (after the in-flight one settles) must truncate any
		// streaming previews created after the view, not the view itself. Its
		// truncation point is `committedEnd`, which the view seeds to its own
		// length.
		const view: SessionView = {
			items: [{ kind: 'user', id: 1, seq: 1, text: 'go' }],
			last_seq: 1,
			turn_running: true,
			runtime_models: []
		};
		let state = stateFromView(view);
		// A live delta creates a streaming preview after the view.
		state = apply(state, { type: 'delta', delta: 'text', index: 0, text: 'preview' });
		expect(state.items).toHaveLength(2);
		// The next RequestStarted truncates the preview, keeping the view intact.
		state = apply(state, reqStarted(2));
		expect(state.requestStart).toBe(1);
		state = apply(state, contentBlock(3, { Text: { text: 'real answer' } }));
		expect(state.items).toHaveLength(2);
		expect(state.items[0]).toMatchObject({ kind: 'user', text: 'go' });
		expect(state.items[1]).toMatchObject({ kind: 'text', text: 'real answer' });
	});
});

// ── Replay boundary (ready gate) ────────────────────────────────────
//
// The view renders nothing until the gateway's replay_end frame folds in —
// the "await replay, then present" sequencing that keeps history from
// visibly scrolling past. These pin the flag's lifecycle: false from a fresh
// state, true exactly at the marker, and unaffected by the events before it.

describe('replay boundary (ready)', () => {
	it('ready is false until the replay_end marker folds in', () => {
		const state = fold([turnStarted(1, 'hi'), reqStarted(2)]);
		expect(state.ready).toBe(false);
	});

	it('replay_end flips ready without disturbing folded content', () => {
		const state = fold([
			turnStarted(1, 'hi'),
			reqStarted(2),
			contentBlock(3, { Text: { text: 'answer' } }),
			{ type: 'replay_end' } as GatewayEvent
		]);
		expect(state.ready).toBe(true);
		expect(state.items.filter((i) => i.kind === 'user')).toHaveLength(1);
		expect(state.items.filter((i) => i.kind === 'text')).toHaveLength(1);
	});

	it('live events after the marker keep ready set', () => {
		const state = fold([
			{ type: 'replay_end' } as GatewayEvent,
			turnStarted(1, 'hi'),
			{ type: 'turn_settled', incomplete: null }
		]);
		expect(state.ready).toBe(true);
	});
});

// ── Streaming-preview replacement (stuck running card) ──────────────
//
// A tool-call preview card (seq=-1, built from live deltas) must be REPLACED
// by its committed ContentBlock, keyed on the shared block `index`. When a
// non-tool block commits first in the same request (flipping requestCommitted),
// the tool's ContentBlock takes the append path — which must still find and
// complete the preview, not push a duplicate that leaves the preview running
// forever (its seq=-1 can never be paired by a Tool::Completed).

describe('streaming preview replacement', () => {
	it('a tool preview after a text commit is replaced by its committed block, not duplicated', () => {
		const state = fold([
			reqStarted(1),
			// text block commits first → requestCommitted = true
			contentBlock(2, { Text: { text: 'let me run a command' } }, 0),
			// model opens a tool call (block index 1) → streaming preview, partial args
			{
				type: 'delta',
				delta: 'block_start',
				index: 1,
				kind: 'tool_call',
				tool: 'shell'
			} as GatewayEvent,
			{ type: 'delta', delta: 'tool_args', index: 1, json: '{"command":"sl' } as GatewayEvent,
			// the tool's committed ContentBlock arrives (same index 1)
			contentBlock(
				3,
				{ ToolCall: { id: 'c1', name: 'shell', arguments: '{"command":"sleep 5"}' } },
				1
			),
			// the tool completes against the committed seq 3
			toolCompleted(4, 3, 'done')
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		// Exactly ONE tool card: the preview was completed in place, not left
		// behind as a stuck running duplicate.
		expect(tools).toHaveLength(1);
		expect(tools[0].kind === 'tool' && tools[0].seq).toBe(3);
		expect(tools[0].kind === 'tool' && tools[0].status).toBe('done');
		expect(tools[0].kind === 'tool' && tools[0].callId).toBe('c1');
	});

	it('concurrent tool previews are each replaced by their own index, not confused', () => {
		const state = fold([
			reqStarted(1),
			contentBlock(2, { Text: { text: 'running two commands' } }, 0),
			// two previews open at indices 1 and 2
			{
				type: 'delta',
				delta: 'block_start',
				index: 1,
				kind: 'tool_call',
				tool: 'shell'
			} as GatewayEvent,
			{
				type: 'delta',
				delta: 'block_start',
				index: 2,
				kind: 'tool_call',
				tool: 'shell'
			} as GatewayEvent,
			{ type: 'delta', delta: 'tool_args', index: 1, json: '{"command":"echo A"}' } as GatewayEvent,
			{ type: 'delta', delta: 'tool_args', index: 2, json: '{"command":"echo B"}' } as GatewayEvent,
			// committed blocks land (index 2 before index 1 — completion order)
			contentBlock(
				3,
				{ ToolCall: { id: 'cB', name: 'shell', arguments: '{"command":"echo B"}' } },
				2
			),
			contentBlock(
				4,
				{ ToolCall: { id: 'cA', name: 'shell', arguments: '{"command":"echo A"}' } },
				1
			),
			toolCompleted(5, 3, 'B done'),
			toolCompleted(6, 4, 'A done')
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		expect(tools).toHaveLength(2);
		// Each preview matched its own committed block by index: no seq=-1 left.
		expect(tools.every((t) => t.kind === 'tool' && t.seq !== -1)).toBe(true);
		expect(tools.every((t) => t.kind === 'tool' && t.status === 'done')).toBe(true);
		const byCallId = new Map(tools.map((t) => [t.kind === 'tool' ? t.callId : '', t]));
		expect(byCallId.get('cA')?.kind === 'tool' && byCallId.get('cA')!.seq).toBe(4);
		expect(byCallId.get('cB')?.kind === 'tool' && byCallId.get('cB')!.seq).toBe(3);
	});

	it('a preview already replaced must not be replaced again by a later same-index block', () => {
		// First-commit truncation slices the preview away and the formal card
		// takes seq>0; a later block reusing the same open[index] must not find
		// a seq=-1 card there (the guard prevents clobbering the formal card).
		const state = fold([
			reqStarted(1),
			// preview at index 0
			{
				type: 'delta',
				delta: 'block_start',
				index: 0,
				kind: 'tool_call',
				tool: 'shell'
			} as GatewayEvent,
			// FIRST commit (index 0) → truncation replaces the preview via the
			// first-commit path; the formal card has seq=2.
			contentBlock(2, { ToolCall: { id: 'c1', name: 'shell', arguments: '{"a":1}' } }, 0),
			toolCompleted(3, 2, 'done')
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		expect(tools).toHaveLength(1);
		expect(tools[0].kind === 'tool' && tools[0].status).toBe('done');
		expect(tools[0].kind === 'tool' && tools[0].result).toBe('done');
	});

	it('a text preview after a reasoning commit is replaced, not duplicated', () => {
		const state = fold([
			reqStarted(1),
			// reasoning commits first → requestCommitted = true
			contentBlock(2, { Reasoning: { text: 'thinking' } }, 0),
			// model streams a text preview (index 1)
			{ type: 'delta', delta: 'text', index: 1, text: 'partial ans' } as GatewayEvent,
			// the text's committed block arrives (append path, index 1)
			contentBlock(3, { Text: { text: 'partial answer, complete' } }, 1)
		]);
		const texts = state.items.filter((i) => i.kind === 'text');
		// Exactly ONE text item: the streaming preview was completed in place.
		expect(texts).toHaveLength(1);
		expect(texts[0].kind === 'text' && texts[0].streaming).toBe(false);
		expect(texts[0].kind === 'text' && texts[0].text).toBe('partial answer, complete');
		expect(texts[0].id).toBe(3);
	});

	it('a reasoning preview after a text commit is replaced, not duplicated', () => {
		const state = fold([
			reqStarted(1),
			// text commits first (provider opened text@0 before reasoning@1)
			contentBlock(2, { Text: { text: 'the answer' } }, 0),
			// reasoning preview streams (index 1)
			{ type: 'delta', delta: 'reasoning', index: 1, text: 'let me th' } as GatewayEvent,
			// reasoning's committed block arrives (index 1)
			contentBlock(3, { Reasoning: { text: 'let me think carefully' } }, 1)
		]);
		const reasoning = state.items.filter((i) => i.kind === 'reasoning');
		expect(reasoning).toHaveLength(1);
		expect(reasoning[0].kind === 'reasoning' && reasoning[0].streaming).toBe(false);
		expect(reasoning[0].kind === 'reasoning' && reasoning[0].text).toBe('let me think carefully');
		expect(reasoning[0].id).toBe(3);
	});
});

// ── Commit-time bookkeeping integrity ─────────────────────────────
//
// Two position books drive all commit/pairing logic: `open` (block index →
// items position of the streaming preview) and `toolSeqs` (committed seq →
// items position, for Tool::Completed pairing). Both hold POSITIONS into
// items[], so any commit-path mutation of the list (truncation, mid-list
// splice) must keep them in sync — a stale position makes pairResult write
// the wrong card (stuck running) or the preview replacement clobber a
// committed card (duplicates).

describe('commit-time bookkeeping integrity', () => {
	it('a truncation resets open: a later same-index commit cannot clobber the card that slid into the stale position', () => {
		// Provider order: tool@1 streams a preview, reasoning@2 streams after.
		// The tool commits FIRST (text@0 was empty and dropped): truncation
		// slices both previews, then the tool card lands at position 0 — the
		// exact slot stale open[1] still pointed at. Without the open reset,
		// the committed reasoning@2 would look up open[2]... and any later
		// same-index block would overwrite the tool card at open[1]=0.
		const state = fold([
			reqStarted(1),
			{
				type: 'delta',
				delta: 'block_start',
				index: 1,
				kind: 'tool_call',
				tool: 'shell'
			} as GatewayEvent,
			{ type: 'delta', delta: 'tool_args', index: 1, json: '{"command":"ls"}' } as GatewayEvent,
			{
				type: 'delta',
				delta: 'block_start',
				index: 2,
				kind: 'reasoning'
			} as GatewayEvent,
			{ type: 'delta', delta: 'reasoning', index: 2, text: 'post-tool thought' } as GatewayEvent,
			// Commits arrive in collector order: tool@1 first (text@0 dropped).
			contentBlock(2, { ToolCall: { id: 'c1', name: 'shell', arguments: '{"command":"ls"}' } }, 1),
			contentBlock(3, { Reasoning: { text: 'post-tool thought' } }, 2),
			toolCompleted(4, 2, 'ok')
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		// Exactly one tool card — the stale-open clobber would have replaced
		// it with a second card or left a duplicate preview behind.
		expect(tools).toHaveLength(1);
		// The completed result must land on the REAL card: toolSeqs was
		// registered at position 0, then the reasoning splice shifted the
		// card to position 1 — only a shifted book still finds it.
		expect(tools[0].kind === 'tool' && tools[0].status).toBe('done');
		expect(tools[0].kind === 'tool' && tools[0].result).toBe('ok');
	});

	it('a reasoning splice shifts toolSeqs: the completed result lands on the tool card, not the spliced-in reasoning', () => {
		// tool commits (position 0, toolSeqs: seq2→0), then a reasoning block
		// commits and splices in at commitBase=0, pushing the card to 1. An
		// unshifted toolSeqs would pair the result onto the reasoning item
		// (a no-op: kind mismatch) and leave the card running forever.
		const state = fold([
			reqStarted(1),
			contentBlock(2, { ToolCall: { id: 'c1', name: 'shell', arguments: '{}' } }, 0),
			contentBlock(3, { Reasoning: { text: 'late reasoning' } }, 1),
			toolCompleted(4, 2, 'done')
		]);
		const tool = state.items.find((i) => i.kind === 'tool');
		expect(tool?.kind === 'tool' && tool.status).toBe('done');
		expect(tool?.kind === 'tool' && tool.result).toBe('done');
		// Reasoning stays at the head (commitBase ordering), the card after it.
		expect(state.items.map((i) => i.kind)).toEqual(['reasoning', 'tool']);
	});
});
