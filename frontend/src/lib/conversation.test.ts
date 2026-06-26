import { describe, it, expect } from 'vitest';
import { apply, emptyState, type ConversationState } from './conversation';
import type { GatewayEvent } from '$lib/types/GatewayEvent';

function fold(events: GatewayEvent[]): ConversationState {
	return events.reduce(apply, emptyState());
}

/** Build a RequestStarted committed event. */
function reqStarted(seq: number): GatewayEvent {
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
					model: 'm',
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

describe('conversation fold', () => {
	// ── Streaming: temporal ordering ───────────────────────────────────

	it('streaming: reasoning appears before text when provider opens text block first', () => {
		// Provider opens text@0 (empty), then reasoning@1, then fills text@0.
		// With temporal ordering, reasoning should appear before text.
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

		// Key assertion: reasoning appears BEFORE text in the items array
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
		// Provider opens text@0 then reasoning@1; collector emits in that order.
		// commitBlock must reorder so reasoning appears before text.
		const events: GatewayEvent[] = [
			reqStarted(1),
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: '' },
			{ type: 'delta', delta: 'block_start', index: 1, kind: 'reasoning', tool: null },
			{ type: 'delta', delta: 'reasoning', index: 1, text: 'thinking...' },
			{ type: 'delta', delta: 'text', index: 0, text: 'answer' },
			// Collector committed order: text first, reasoning second
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
		// Two separate blocks despite both being index 0 — the reset prevents the
		// second round from extending the first round's block.
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
});
