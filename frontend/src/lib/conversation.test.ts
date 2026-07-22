import { describe, it, expect } from 'vitest';
import { apply, emptyState, type ConversationState } from './conversation';
import type { GatewayEvent } from '$lib/types/GatewayEvent';

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
		| { ToolCall: { id: string; name: string; arguments: string } }
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
				ContentBlock: { request_id: 'r', index: 0, content }
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

// ── Plan control tool: folded into structured plan cards ───────────────
//
// `plan` is a control tool (src/agent/plan.rs): its calls arrive as committed
// ToolCall ContentBlocks whose `arguments` is a PlanOp JSON. The fold must turn
// these into a single structured `plan` item (one card per `init`, later ops
// mutating it in place) — NOT a generic tool block per op.

/** A committed `plan` tool call carrying the given op JSON as its arguments. */
function planCall(seq: number, op: object): GatewayEvent {
	return contentBlock(seq, {
		ToolCall: { id: `p${seq}`, name: 'plan', arguments: JSON.stringify(op) }
	});
}

/** Pull the single plan card's steps (asserts exactly one exists). */
function planSteps(state: ConversationState) {
	const plans = state.items.filter((i) => i.kind === 'plan');
	expect(plans).toHaveLength(1);
	const p = plans[0];
	return p.kind === 'plan' ? p.steps : [];
}

describe('plan fold', () => {
	it('init creates one plan card with sequential pending steps (no tool blocks)', () => {
		const state = fold([
			reqStarted(1),
			planCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }, { content: 'c' }] })
		]);
		// A plan call must never surface as a generic tool block.
		expect(state.items.filter((i) => i.kind === 'tool')).toHaveLength(0);
		const steps = planSteps(state);
		expect(steps.map((s) => s.id)).toEqual(['1', '2', '3']);
		expect(steps.every((s) => s.status === 'pending')).toBe(true);
	});

	it('start/complete mutate the existing card in place, not append new cards', () => {
		const state = fold([
			reqStarted(1),
			planCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }] }),
			reqStarted(3),
			planCall(4, { op: 'start', id: '1' }),
			reqStarted(5),
			planCall(6, { op: 'complete', id: '1' })
		]);
		const steps = planSteps(state);
		expect(steps[0].status).toBe('completed');
		expect(steps[1].status).toBe('pending');
	});

	it('cancel and block record their reason', () => {
		const state = fold([
			reqStarted(1),
			planCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }] }),
			reqStarted(3),
			planCall(4, { op: 'cancel', id: '1', reason: 'no such tool' }),
			reqStarted(5),
			planCall(6, { op: 'block', id: '2', reason: 'needs API key' })
		]);
		const steps = planSteps(state);
		expect(steps[0].status).toBe('cancelled');
		expect(steps[0].reason).toBe('no such tool');
		expect(steps[1].status).toBe('blocked');
		expect(steps[1].reason).toBe('needs API key');
	});

	it('add appends at end and inserts after an anchor with max+1 id', () => {
		const state = fold([
			reqStarted(1),
			planCall(2, { op: 'init', steps: [{ content: 'a' }, { content: 'b' }] }),
			reqStarted(3),
			planCall(4, { op: 'add', content: 'end' }),
			reqStarted(5),
			planCall(6, { op: 'add', content: 'mid', after_id: '1' })
		]);
		const steps = planSteps(state);
		expect(steps.map((s) => s.content)).toEqual(['a', 'mid', 'b', 'end']);
		// id is max+1, unaffected by insert position
		const mid = steps.find((s) => s.content === 'mid');
		expect(mid?.id).toBe('4');
	});

	it('a non-init op with no prior plan is a benign no-op', () => {
		const state = fold([reqStarted(1), planCall(2, { op: 'start', id: '1' })]);
		expect(state.items.filter((i) => i.kind === 'plan')).toHaveLength(0);
		expect(state.items.filter((i) => i.kind === 'tool')).toHaveLength(0);
	});

	it('start on an unknown id leaves the plan unchanged', () => {
		const state = fold([
			reqStarted(1),
			planCall(2, { op: 'init', steps: [{ content: 'a' }] }),
			reqStarted(3),
			planCall(4, { op: 'start', id: '99' })
		]);
		expect(planSteps(state)[0].status).toBe('pending');
	});

	it('a second init starts a fresh card, preserving the first as history', () => {
		const state = fold([
			reqStarted(1),
			planCall(2, { op: 'init', steps: [{ content: 'a' }] }),
			reqStarted(3),
			planCall(4, { op: 'complete', id: '1' }),
			reqStarted(5),
			planCall(6, { op: 'init', steps: [{ content: 'x' }, { content: 'y' }] })
		]);
		const plans = state.items.filter((i) => i.kind === 'plan');
		expect(plans).toHaveLength(2);
		expect(plans[0].kind === 'plan' && plans[0].steps[0].status).toBe('completed');
		expect(plans[1].kind === 'plan' && plans[1].steps.map((s) => s.content)).toEqual(['x', 'y']);
	});

	it('streaming plan placeholder is replaced by the committed card (no duplication)', () => {
		const state = fold([
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'tool_call', tool: 'plan' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: '{"op":"in' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: 'it","steps":[{"content":"a"}]}' },
			planCall(2, { op: 'init', steps: [{ content: 'a' }] })
		]);
		const plans = state.items.filter((i) => i.kind === 'plan');
		expect(plans).toHaveLength(1);
		expect(plans[0].kind === 'plan' && plans[0].streaming).toBe(false);
		expect(plans[0].kind === 'plan' && plans[0].steps[0].content).toBe('a');
	});

	it('a later turn op mutates a plan card from an earlier request', () => {
		// The card lives before the second RequestStarted; commitBlock truncation
		// must not lose it, and the op must still target it.
		const state = fold([
			turnStarted(1, 'go'),
			reqStarted(2),
			planCall(3, { op: 'init', steps: [{ content: 'a' }] }),
			turnCompleted(4),
			turnStarted(5, 'continue'),
			reqStarted(6),
			planCall(7, { op: 'complete', id: '1' })
		]);
		const steps = planSteps(state);
		expect(steps[0].status).toBe('completed');
	});
});

function permRequested(seq: number, callId: string, tool: string): GatewayEvent {
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
			Permission: { Requested: { call_id: callId, tool_name: tool, input: { path: 'x.txt' } } }
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

// ── File cache: populated by read/write, never by edit ─────────────────────
//
// `diff-builder.ts` needs pre-edit file content to render contextual diffs for
// `edit`/`write` cards. The cache is fed only by committed `read` results and
// `write` args — never by `edit`, since edit's own diff rendering needs the
// PRE-edit content that only read/write leave here (`doc/tool-protocol.md`
// §11.4).

describe('file cache fold', () => {
	it('a full-file read result populates the cache', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"a.txt"}' } }),
			toolCompleted(2, 1, '[a.txt]\n1:hello\n2:world')
		]);
		expect(state.fileCache.get('a.txt')).toEqual(['hello', 'world']);
	});

	it("write's args populate the cache the moment the call commits, before its result arrives", () => {
		const state = fold([
			contentBlock(1, {
				ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"b.txt","content":"x\\ny"}' }
			})
		]);
		expect(state.fileCache.get('b.txt')).toEqual(['x', 'y']);
		const t = state.items.find((i) => i.kind === 'tool');
		expect(t?.kind === 'tool' && t.status).toBe('running'); // result hasn't landed yet
	});

	it('edit does not touch the cache — its own diff render needs the pre-edit content', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"a.txt"}' } }),
			toolCompleted(2, 1, '[a.txt]\n1:hello\n2:world'),
			contentBlock(3, {
				ToolCall: {
					id: 'c2',
					name: 'edit',
					arguments: '{"edits":[{"path":"a.txt","old":["hello"],"new":["HELLO"]}]}'
				}
			}),
			toolCompleted(4, 3, 'edited a.txt (1 replacement)')
		]);
		expect(state.fileCache.get('a.txt')).toEqual(['hello', 'world']);
	});

	it('a ranged read result does not clobber a previously-cached full-file read', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"a.txt"}' } }),
			toolCompleted(2, 1, '[a.txt]\n1:hello\n2:world\n3:!'),
			contentBlock(3, {
				ToolCall: {
					id: 'c2',
					name: 'read',
					arguments: '{"path":"a.txt","range":{"start":2,"end":2}}'
				}
			}),
			toolCompleted(4, 3, '[a.txt]\n2:world')
		]);
		// The ranged result alone would look like a 1-line file; the full-file
		// cache entry from the first read must survive untouched.
		expect(state.fileCache.get('a.txt')).toEqual(['hello', 'world', '!']);
	});

	it('a directory-listing read result is not mistaken for file content', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"."}' } }),
			toolCompleted(2, 1, '[./]\na.txt\nb.txt')
		]);
		expect(state.fileCache.has('.')).toBe(false);
	});

	it('a failed read does not populate the cache', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"nope.txt"}' } }),
			{
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
								content: [{ Text: 'failed to read nope.txt' }],
								is_error: true,
								error_code: 'read_failed'
							}
						}
					}
				}
			} as unknown as GatewayEvent
		]);
		expect(state.fileCache.has('nope.txt')).toBe(false);
	});

	it('replaying the same event history twice yields an identical cache (pure fold)', () => {
		const events: GatewayEvent[] = [
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"a.txt"}' } }),
			toolCompleted(2, 1, '[a.txt]\n1:hello'),
			contentBlock(3, {
				ToolCall: { id: 'c2', name: 'write', arguments: '{"path":"b.txt","content":"x"}' }
			})
		];
		const first = fold(events);
		const second = fold(events);
		expect([...second.fileCache.entries()]).toEqual([...first.fileCache.entries()]);
		expect(second.fileCache.get('a.txt')).toEqual(['hello']);
		expect(second.fileCache.get('b.txt')).toEqual(['x']);
	});

	// A second `write` to the same path advances the cache to ITS new content
	// the moment it commits — by the time WriteResult renders, the cache no
	// longer holds what the file looked like before this call. `prevLines`
	// captures that "before" snapshot on the item itself at commit time, before
	// the cache moves on, so the overwrite diff has something to diff against.
	it('captures the pre-write snapshot as prevLines, since the cache has already moved on by render time', () => {
		const state = fold([
			contentBlock(1, {
				ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"a.txt","content":"one\\ntwo"}' }
			}),
			toolCompleted(2, 1, 'wrote a.txt (new, 2 lines)'),
			contentBlock(3, {
				ToolCall: { id: 'c2', name: 'write', arguments: '{"path":"a.txt","content":"ONE\\ntwo"}' }
			})
		]);
		const tools = state.items.filter((i) => i.kind === 'tool');
		expect(tools).toHaveLength(2);
		// First write: no prior content — nothing to snapshot.
		expect(tools[0].kind === 'tool' && tools[0].prevLines).toBeUndefined();
		// Second write: the cache held the FIRST write's content at the moment
		// this call committed — captured here, even though fileCache itself has
		// since advanced to this call's own new content.
		expect(tools[1].kind === 'tool' && tools[1].prevLines).toEqual(['one', 'two']);
		expect(state.fileCache.get('a.txt')).toEqual(['ONE', 'two']);
	});

	// A failed write never touches disk, but its commit already advanced the
	// cache to the args' content (so the streaming preview could diff). The
	// result pairing must roll that back — otherwise the cache holds phantom
	// content and a later edit to the same file renders a confidently-wrong
	// diff against text that was never written.
	it('a failed write rolls the cache back to its pre-write content', () => {
		const state = fold([
			contentBlock(1, { ToolCall: { id: 'c1', name: 'read', arguments: '{"path":"a.txt"}' } }),
			toolCompleted(2, 1, '[a.txt]\n1:hello\n2:world'),
			contentBlock(3, {
				ToolCall: { id: 'c2', name: 'write', arguments: '{"path":"a.txt","content":"phantom"}' }
			}),
			{
				type: 'event',
				schema_version: 'ominiforge.event.v1',
				seq: 4,
				session_id: 's',
				timestamp: '2026-06-24T00:00:00Z',
				source: { kind: 'Runtime', id: 'runtime' },
				parent_event_id: null,
				turn_id: null,
				payload: {
					Tool: {
						Completed: {
							tool_call_event_id: { session_id: 's', seq: 3 },
							result: {
								content: [{ Text: 'failed to write a.txt' }],
								is_error: true,
								error_code: 'write_failed'
							}
						}
					}
				}
			} as unknown as GatewayEvent
		]);
		expect(state.fileCache.get('a.txt')).toEqual(['hello', 'world']);
	});

	// The new-file case of the same rollback: the commit created the cache key
	// (no pre-write snapshot existed), so the denial must remove it entirely —
	// a surviving phantom-file entry would poison a later edit just the same.
	it('a denied write of a new file drops the cache key entirely', () => {
		const state = fold([
			contentBlock(1, {
				ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"b.txt","content":"x"}' }
			}),
			toolFailed(2, 1, 'denied_by_user')
		]);
		expect(state.fileCache.has('b.txt')).toBe(false);
	});

	// The write path must split content into the same lines shape a read of
	// those bytes produces (the backend's `str::lines`): no phantom trailing
	// line from a final newline, `\r` stripped. A different shape would make a
	// write→edit chain match tail lines against stale content.
	it("write content splits exactly like a read of the same bytes (trailing newline, CRLF)", () => {
		const state = fold([
			contentBlock(1, {
				ToolCall: {
					id: 'c1',
					name: 'write',
					arguments: '{"path":"w.txt","content":"a\\r\\nb\\n"}'
				}
			}),
			contentBlock(2, { ToolCall: { id: 'c2', name: 'read', arguments: '{"path":"r.txt"}' } }),
			toolCompleted(3, 2, '[r.txt]\n1:a\n2:b')
		]);
		expect(state.fileCache.get('w.txt')).toEqual(['a', 'b']);
		expect(state.fileCache.get('w.txt')).toEqual(state.fileCache.get('r.txt'));
	});

	// ── Approval mid-stream: the existing orphanTools mechanism, not new code ──

	it('a permission ask that arrives mid-args-stream still resolves via the orphan mechanism, and the cache still populates on commit', () => {
		const events: GatewayEvent[] = [
			reqStarted(0),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'tool_call', tool: 'write' },
			{ type: 'delta', delta: 'tool_args', index: 0, json: '{"path":"x.txt"' },
			permRequested(1, 'c1', 'write'),
			{ type: 'delta', delta: 'tool_args', index: 0, json: ',"content":"hi"}' },
			contentBlock(2, {
				ToolCall: { id: 'c1', name: 'write', arguments: '{"path":"x.txt","content":"hi"}' }
			}),
			permDecided(3, 'c1', 'Approved'),
			toolCompleted(4, 2, 'wrote x.txt (new, 1 lines)')
		];
		const state = fold(events);
		const tools = state.items.filter((i) => i.kind === 'tool');
		expect(tools).toHaveLength(1);
		const t = tools[0];
		expect(t.kind === 'tool' && t.approvalPending).toBe(false);
		expect(t.kind === 'tool' && t.status).toBe('done');
		expect(t.kind === 'tool' && t.result).toContain('wrote x.txt');
		expect(state.orphanTools.size).toBe(0);
		expect(state.fileCache.get('x.txt')).toEqual(['hi']);
	});
});
