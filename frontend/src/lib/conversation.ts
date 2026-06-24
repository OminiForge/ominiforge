// Folds a session's GatewayEvent stream into a flat, render-ready list of
// conversation items. The gateway sends two interleaved kinds (gateway.md §4):
//
//   - committed `event` frames (persisted CoreEvent) — the authoritative record
//   - live `delta` frames — token-level streaming, ephemeral, not replayed
//
// Streaming blocks are keyed by their content-block `index` within the current
// model request — NOT by list position — because the provider interleaves
// blocks (e.g. a text block and a reasoning block grow concurrently). The
// committed ModelEvent::ContentBlock for an index is the final authoritative
// version and replaces the streaming preview at that index. Indices reset each
// model request, so we clear the index→position map on RequestStarted. On
// reconnect only committed events replay, so the committed copy is always the
// source of truth and deltas are pure UX sugar.

import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { CoreEvent } from '$lib/types/CoreEvent';
import type { BlockContent } from '$lib/types/BlockContent';

export type Item =
	| { kind: 'user'; text: string }
	| { kind: 'text'; text: string; streaming: boolean }
	| { kind: 'reasoning'; text: string; streaming: boolean }
	| { kind: 'tool_call'; name: string; args: string; streaming: boolean }
	| { kind: 'tool_result'; name: string; ok: boolean; text: string }
	| { kind: 'error'; message: string }
	| { kind: 'notice'; message: string };

export interface ConversationState {
	items: Item[];
	/** Latest seq seen from a committed event, for reconnect resume. */
	lastSeq?: number;
	/** Set when the stream reports a turn settled; carries any incomplete reason. */
	lastSettle?: string | null;
	/** Block index → items position, for the current model request only. */
	open: Record<number, number>;
}

export function emptyState(): ConversationState {
	return { items: [], open: {} };
}

/**
 * Apply one GatewayEvent to the state, returning a new state (immutable so
 * Svelte re-renders on assignment). Exhaustive over the GatewayEvent union;
 * an unhandled variant is a compile error (the `never` arm).
 */
export function apply(state: ConversationState, ev: GatewayEvent): ConversationState {
	switch (ev.type) {
		case 'event':
			return applyCommitted(state, ev);
		case 'delta':
			return applyDelta(state, ev);
		case 'turn_settled':
			return { ...state, lastSettle: ev.incomplete };
		case 'compacted':
			// The caller resubscribes to ev.new_session_id; surface a marker.
			return push(state, { kind: 'notice', message: `compacted → ${ev.new_session_id}` });
		case 'notice':
			return push(state, { kind: 'notice', message: ev.message });
		default:
			return assertNever(ev);
	}
}

/** A committed CoreEvent: the durable, authoritative record. */
function applyCommitted(
	state: ConversationState,
	ev: GatewayEvent & { type: 'event' }
): ConversationState {
	// The flattened CoreEvent fields live alongside `type` on the same object.
	const core = ev as unknown as CoreEvent & { seq: number };
	const next: ConversationState = { ...state, lastSeq: core.seq };

	const payload = core.payload;
	if ('Turn' in payload) {
		const t = payload.Turn;
		if ('Started' in t && t.Started.input) {
			return push(next, { kind: 'user', text: t.Started.input });
		}
		return next;
	}
	if ('Model' in payload) {
		const m = payload.Model;
		// A new request resets block indices; clear the open-block map so the next
		// round's index 0 does not collide with this round's.
		if ('RequestStarted' in m) return { ...next, open: {} };
		if ('ContentBlock' in m) {
			return commitBlock(next, m.ContentBlock.index, m.ContentBlock.content);
		}
		return next;
	}
	if ('Tool' in payload) {
		const tool = payload.Tool;
		if ('Completed' in tool) {
			const out = tool.Completed.result;
			const text = out.content
				.map((c) => ('Text' in c ? c.Text : `[${'Image' in c ? 'image' : 'artifact'}]`))
				.join('');
			return push(next, { kind: 'tool_result', name: '', ok: !out.is_error, text });
		}
		if ('Failed' in tool) {
			return push(next, {
				kind: 'tool_result',
				name: '',
				ok: false,
				text: tool.Failed.error.message
			});
		}
		return next;
	}
	if ('Error' in payload) {
		return push(next, { kind: 'error', message: payload.Error.Raised.message });
	}
	// Session / Artifact / Injection / Hook events are not rendered in the
	// conversation transcript (they show up in the monitor view instead).
	return next;
}

/** Finalize the streaming block at `index` with its committed content. */
function commitBlock(
	state: ConversationState,
	index: number,
	content: BlockContent
): ConversationState {
	let item: Item;
	if ('Text' in content) item = { kind: 'text', text: content.Text.text, streaming: false };
	else if ('Reasoning' in content)
		item = { kind: 'reasoning', text: content.Reasoning.text, streaming: false };
	else
		item = {
			kind: 'tool_call',
			name: content.ToolCall.name,
			args: content.ToolCall.arguments,
			streaming: false
		};

	const pos = state.open[index];
	const items = [...state.items];
	if (pos !== undefined) {
		items[pos] = item; // replace the streaming preview in place
		return { ...state, items };
	}
	// No preview seen (e.g. replay without deltas): append the committed block.
	items.push(item);
	return { ...state, items };
}

/** A live token-level delta: open or extend the streaming block at its index. */
function applyDelta(
	state: ConversationState,
	ev: GatewayEvent & { type: 'delta' }
): ConversationState {
	const items = [...state.items];
	const open = { ...state.open };

	const openBlock = (item: Item): ConversationState => {
		open[ev.index] = items.length;
		items.push(item);
		return { ...state, items, open };
	};
	const extend = (next: Item): ConversationState => {
		const pos = open[ev.index];
		if (pos === undefined) {
			items.push(next);
			open[ev.index] = items.length - 1;
		} else {
			items[pos] = next;
		}
		return { ...state, items, open };
	};
	const at = (): Item | undefined => {
		const pos = open[ev.index];
		return pos === undefined ? undefined : items[pos];
	};

	switch (ev.delta) {
		case 'block_start': {
			const kind =
				ev.kind === 'reasoning' ? 'reasoning' : ev.kind === 'tool_call' ? 'tool_call' : 'text';
			if (kind === 'tool_call')
				return openBlock({ kind: 'tool_call', name: ev.tool ?? '', args: '', streaming: true });
			if (kind === 'reasoning') return openBlock({ kind: 'reasoning', text: '', streaming: true });
			return openBlock({ kind: 'text', text: '', streaming: true });
		}
		case 'text': {
			const cur = at();
			if (cur && cur.kind === 'text') return extend({ ...cur, text: cur.text + ev.text });
			return extend({ kind: 'text', text: ev.text, streaming: true });
		}
		case 'reasoning': {
			const cur = at();
			if (cur && cur.kind === 'reasoning') return extend({ ...cur, text: cur.text + ev.text });
			return extend({ kind: 'reasoning', text: ev.text, streaming: true });
		}
		case 'tool_args': {
			const cur = at();
			if (cur && cur.kind === 'tool_call') return extend({ ...cur, args: cur.args + ev.json });
			return extend({ kind: 'tool_call', name: '', args: ev.json, streaming: true });
		}
		default:
			return assertNever(ev);
	}
}

function push(state: ConversationState, item: Item): ConversationState {
	return { ...state, items: [...state.items, item] };
}

function assertNever(x: never): never {
	throw new Error(`unhandled variant: ${JSON.stringify(x)}`);
}
