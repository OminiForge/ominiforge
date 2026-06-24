import { describe, it, expect } from 'vitest';
import { apply, emptyState, type ConversationState } from './conversation';
import type { GatewayEvent } from '$lib/types/GatewayEvent';

function fold(events: GatewayEvent[]): ConversationState {
	return events.reduce(apply, emptyState());
}

describe('conversation fold', () => {
	it('keeps interleaved text and reasoning blocks separate by index', () => {
		// Captured from a real gateway turn: a text block (index 0) and a
		// reasoning block (index 1) stream concurrently. A position/tail-based
		// fold would split the text into two items; index-keyed folding must not.
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
	});

	it('committed ContentBlock replaces the streaming preview, not appends', () => {
		const events: GatewayEvent[] = [
			{ type: 'delta', delta: 'block_start', index: 0, kind: 'text', tool: null },
			{ type: 'delta', delta: 'text', index: 0, text: 'Hi th' },
			// committed event (flattened CoreEvent) finalizes index 0
			{
				type: 'event',
				schema_version: 'ominiforge.event.v1',
				seq: 3,
				session_id: 's',
				timestamp: '2026-06-24T00:00:00Z',
				source: { kind: 'Model', id: 'm' },
				parent_event_id: null,
				turn_id: null,
				payload: {
					Model: {
						ContentBlock: { request_id: 'r', index: 0, content: { Text: { text: 'Hi there' } } }
					}
				}
			} as unknown as GatewayEvent
		];

		const text = fold(events).items.filter((i) => i.kind === 'text');
		expect(text).toHaveLength(1);
		expect(text[0].kind === 'text' && text[0].streaming).toBe(false);
		expect(text[0].kind === 'text' && text[0].text).toBe('Hi there');
	});

	it('a new model request resets block indices', () => {
		const reqStarted = (seq: number): GatewayEvent =>
			({
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
			}) as unknown as GatewayEvent;

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
});
