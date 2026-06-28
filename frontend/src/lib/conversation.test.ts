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
	content: { Text: { text: string } } | { Reasoning: { text: string } } | { ToolCall: { id: string; name: string; arguments: string } }
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
});
