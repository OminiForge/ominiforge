import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { CoreEvent } from '$lib/types/CoreEvent';
import type { BlockContent } from '$lib/types/BlockContent';

/** The control tool whose calls drive the plan card. Must match
 *  `PLAN_TOOL_NAME` in `src/agent/plan.rs` — plan calls are folded into a
 *  structured plan card instead of rendered as generic tool blocks. */
export const PLAN_TOOL_NAME = 'plan';

/** Step lifecycle, mirroring `StepStatus` in `src/agent/plan.rs`. Terminal:
 *  completed/cancelled/blocked; non-terminal: pending/in_progress. */
export type PlanStatus = 'pending' | 'in_progress' | 'completed' | 'cancelled' | 'blocked';

/** One plan step, mirroring `PlanStep` in `src/agent/plan.rs`. `reason` is
 *  carried by cancelled/blocked steps (the why). */
export interface PlanStep {
	id: string;
	content: string;
	status: PlanStatus;
	reason?: string;
}

export type Item =
	| { kind: 'user'; text: string }
	| { kind: 'text'; text: string; streaming: boolean }
	| { kind: 'reasoning'; text: string; streaming: boolean }
	| {
			kind: 'tool';
			seq: number;
			name: string;
			args: string;
			status: 'running' | 'done' | 'error';
			result?: string;
	  }
	/** A plan checklist, folded from `plan` control-tool calls (one card per
	 *  `init`). `streaming` marks a placeholder shown while the call's args are
	 *  still streaming (partial JSON, not yet foldable); it is replaced by the
	 *  real card on commit. See foldPlanOp. */
	| { kind: 'plan'; steps: PlanStep[]; streaming: boolean }
	| { kind: 'error'; message: string }
	| { kind: 'notice'; message: string };

export interface ConversationState {
	items: Item[];
	lastSeq?: number;
	lastSettle?: string | null;
	/** block index → items position, current request streaming. Only used for tool_call tracking;
	 *  text/reasoning use temporal (append-at-end) ordering to match TUI behavior. */
	open: Record<number, number>;
	/** Position in items[] where current request's blocks start (set on RequestStarted).
	 *  Used once by commitBlock to truncate streaming previews, then stays defined
	 *  until the next RequestStarted or turn_settled clears it. */
	requestStart?: number;
	/** True after the first committed ContentBlock has truncated streaming previews.
	 *  Prevents re-truncation on subsequent ContentBlocks in the same request. */
	requestCommitted?: boolean;
	/** Insertion point for reasoning items during commit. Ensures reasoning
	 *  is always placed before text, regardless of the collector's block order. */
	commitBase?: number;
	/** End position of committed (non-streaming) items.  Used as the truncation
	 *  point on RequestStarted so that streaming items created *before* the
	 *  late-arriving RequestStarted are also removed.  Only advanced by push()
	 *  when no streaming items are present, preventing a race from corrupting
	 *  the boundary. */
	committedEnd?: number;
	/** committed tool_call seq → items position, for pairing Tool::Completed */
	toolSeqs: Map<number, number>;
	/** Whether a turn is currently running. Driven last-write-wins by the turn
	 *  lifecycle: committed `Turn::Started`/`Resumed` set it, committed
	 *  `Completed`/`Failed`/`Interrupted` and live `turn_settled`/`notice`/
	 *  `compacted` clear it. Folding committed turn events (not the live-only
	 *  `TurnSettled`) is what lets it reconstruct correctly on history replay —
	 *  a finished turn replays its committed `Completed`, so the flag lands
	 *  `false`. Only the Cancel control reads it: cancel is meaningful solely
	 *  while a turn runs (`src/gateway/actor.rs` ignores Cancel when idle), so
	 *  the button hides otherwise. Known gap: a turn ended by Cancel aborts the
	 *  task without persisting a terminator, so reloading such a session leaves
	 *  this `true` (a stale Cancel that no-ops on click). */
	turnRunning?: boolean;
	/** Distinct models seen on the runtime layer: every model a `RequestStarted`
	 *  actually used this session (deduplicated). The display source stays the
	 *  config layer (the session page's `runtime`); this is the *validation*
	 *  source — a model here that isn't the configured one (a subagent/fork using
	 *  something else) is surfaced as a fail-loud divergence, not silently shown
	 *  (`doc/frontend.md` B4, CLAUDE.md #12). It deliberately does not drive the
	 *  INFO Model row, so that row never flickers as subagents switch models. */
	runtimeModels: Set<string>;
}

export function emptyState(): ConversationState {
	return { items: [], open: {}, toolSeqs: new Map(), runtimeModels: new Set() };
}

export function apply(state: ConversationState, ev: GatewayEvent): ConversationState {
	switch (ev.type) {
		case 'event':    return applyCommitted(state, ev);
		case 'delta':    return applyDelta(state, ev);
		case 'turn_settled': return {
			...state,
			lastSettle: ev.incomplete,
			turnRunning: false,
			requestStart: undefined,
			requestCommitted: undefined,
			commitBase: undefined
		};
		case 'compacted':    return push({ ...state, turnRunning: false }, { kind: 'notice', message: `compacted → ${ev.new_session_id}` });
		case 'notice':       return push({ ...state, turnRunning: false }, { kind: 'notice', message: ev.message });
		// Live-only context occupancy snapshot: handled by the page (STATS panel),
		// not folded into conversation items.
		case 'context_updated': return state;
		default: return assertNever(ev);
	}
}

function applyCommitted(
	state: ConversationState,
	ev: GatewayEvent & { type: 'event' }
): ConversationState {
	const core = ev as unknown as CoreEvent & { seq: number };
	const next: ConversationState = { ...state, lastSeq: Number(core.seq) };
	const payload = core.payload;

	if ('Turn' in payload) {
		const t = payload.Turn;
		if ('Started' in t) {
			const started = { ...next, turnRunning: true };
			return t.Started.input
				? push(started, { kind: 'user', text: t.Started.input })
				: started;
		}
		if ('Resumed' in t) return { ...next, turnRunning: true };
		if ('Completed' in t || 'Failed' in t || 'Interrupted' in t)
			return { ...next, turnRunning: false };
		return next;
	}
	if ('Model' in payload) {
		const m = payload.Model;
		if ('RequestStarted' in m) {
			// Record the runtime-layer model for divergence validation (B4). Clone
			// the set only when this model is new, keeping the fold a pure reducer
			// without churning allocations on every request.
			const model = m.RequestStarted.model;
			const runtimeModels = next.runtimeModels.has(model)
				? next.runtimeModels
				: new Set(next.runtimeModels).add(model);
			return {
				...next,
				runtimeModels,
				open: {},
				// Use committedEnd (not items.length) so streaming items created
				// before this late-arriving event are also truncated away.
				requestStart: next.committedEnd ?? 0,
				requestCommitted: false,
				commitBase: undefined
			};
		}
		if ('ContentBlock' in m)
			return commitBlock(next, Number(core.seq), m.ContentBlock.content);
		return next;
	}
	if ('Tool' in payload) {
		const tool = payload.Tool;
		if ('Completed' in tool)
			return pairResult(next, Number(tool.Completed.tool_call_event_id.seq), tool.Completed.result, false);
		if ('Failed' in tool)
			return pairResult(
				next,
				Number(tool.Failed.tool_call_event_id.seq),
				{ content: [{ Text: tool.Failed.error.message }], is_error: true, error_code: null },
				true
			);
		return next;
	}
	if ('Error' in payload)
		return push(next, { kind: 'error', message: payload.Error.Raised.message });
	return next;
}

/// Finalize streaming previews with authoritative committed content.
///
/// Strategy: on the FIRST committed ContentBlock for a request, truncate all
/// streaming previews and rebuild from committed blocks only. Subsequent
/// committed blocks append (with reasoning inserted before text via commitBase).
///
/// Reasoning-before-text ordering is critical because some providers open a text
/// block first (index 0) then reasoning (index 1); the collector preserves that
/// order in committed events, but the user expects reasoning above text.
///
/// When `requestStart` has been cleared (by `turn_settled` arriving before
/// committed events — an async event-forwarding race in the backend), we
/// detect and remove any lingering streaming items to prevent duplication.
function commitBlock(
	state: ConversationState,
	seq: number,
	content: BlockContent
): ConversationState {
	let items: Item[];
	let commitBase = state.commitBase;

	if (state.requestStart !== undefined && !state.requestCommitted) {
		// First commit: replace all streaming previews with authoritative committed stream.
		items = state.items.slice(0, state.requestStart);
		commitBase = state.requestStart;
	} else if (state.requestStart === undefined) {
		// requestStart was cleared (e.g. by turn_settled arriving before the
		// ContentBlock events — a backend event-forwarding race).  Strip any
		// trailing streaming items so committed content replaces them rather
		// than duplicating.
		const firstStreaming = state.items.findIndex(
			(i) => 'streaming' in i && i.streaming
		);
		if (firstStreaming >= 0) {
			items = state.items.slice(0, firstStreaming);
			commitBase = firstStreaming;
		} else {
			items = [...state.items];
		}
	} else {
		items = [...state.items];
	}

	let item: Item;
	if ('Text' in content) {
		if (!content.Text.text.trim()) return { ...state, requestCommitted: true, commitBase, committedEnd: items.length };
		item = { kind: 'text', text: content.Text.text, streaming: false };
		items.push(item);
	} else if ('Reasoning' in content) {
		if (!content.Reasoning.text.trim()) return { ...state, requestCommitted: true, commitBase, committedEnd: items.length };
		item = { kind: 'reasoning', text: content.Reasoning.text, streaming: false };
		// Insert reasoning at commitBase so it appears before any text items
		// that the collector emitted earlier (providers may open text@0 before reasoning@1).
		const insertAt = commitBase ?? items.length;
		items.splice(insertAt, 0, item);
		commitBase = insertAt + 1;
	} else {
		// Plan is a control tool: fold its op into a plan card instead of
		// rendering a generic tool block. The card lives where `init` lands and
		// later ops mutate it in place (mirrors the backend's single authoritative
		// plan, but each `init` starts a fresh card so turn history is preserved).
		if (content.ToolCall.name === PLAN_TOOL_NAME) {
			items = foldPlanOp(items, content.ToolCall.arguments);
			return { ...state, items, requestCommitted: true, commitBase, committedEnd: items.length };
		}
		const toolSeqs = new Map(state.toolSeqs);
		toolSeqs.set(seq, items.length);
		item = { kind: 'tool', seq, name: content.ToolCall.name, args: content.ToolCall.arguments, status: 'running' };
		items.push(item);
		return { ...state, items, toolSeqs, requestCommitted: true, commitBase, committedEnd: items.length };
	}

	return { ...state, items, requestCommitted: true, commitBase, committedEnd: items.length };
}

/// Decoded `plan` op, mirroring `PlanOp` in `src/agent/plan.rs` (externally
/// tagged on `op`). Only the fields each op needs are read; the rest are
/// ignored, matching serde's tolerance on the wire.
type PlanOp =
	| { op: 'init'; steps?: Array<{ content: string }> }
	| { op: 'start'; id: string }
	| { op: 'complete'; id: string }
	| { op: 'cancel'; id: string; reason?: string }
	| { op: 'block'; id: string; reason?: string }
	| { op: 'add'; content: string; after_id?: string };

/// Fold one committed `plan` tool call into the items list.
///
/// Strategy mirrors the backend (`src/agent/plan.rs`): `init` replaces the plan
/// with a fresh card; every other op mutates the *latest* plan card in place.
/// The frontend keeps one card per `init` (not a single global plan) so the
/// conversation preserves the plan of each turn as history — the newest card is
/// always the live one that subsequent ops target.
///
/// Robustness: the args are authoritative committed JSON, but a malformed op or
/// a mutation with no card to target is ignored (the items list is returned
/// unchanged), mirroring the backend's benign `is_error` handling — the model
/// corrects itself next round and we never throw mid-fold.
function foldPlanOp(items: Item[], args: string): Item[] {
	let op: PlanOp;
	try {
		op = JSON.parse(args) as PlanOp;
	} catch {
		return items;
	}
	if (!op || typeof op !== 'object' || typeof op.op !== 'string') return items;

	if (op.op === 'init') {
		const steps: PlanStep[] = (op.steps ?? []).map((s, i) => ({
			id: String(i + 1),
			content: s.content,
			status: 'pending'
		}));
		return [...items, { kind: 'plan', steps, streaming: false }];
	}

	// Mutate the latest plan card. No card → benign no-op (model misused plan).
	const pos = lastPlanIndex(items);
	if (pos === -1) return items;
	const card = items[pos];
	if (card.kind !== 'plan') return items;
	const steps = applyPlanOp(card.steps, op);
	if (steps === card.steps) return items; // unchanged (unknown id / anchor)
	const next = [...items];
	next[pos] = { kind: 'plan', steps, streaming: false };
	return next;
}

/// Apply a non-init op to a step list, returning a new list (or the same
/// reference unchanged when the target id/anchor is absent — a benign no-op
/// matching the backend's `PlanError` → `is_error` path).
function applyPlanOp(steps: PlanStep[], op: PlanOp): PlanStep[] {
	switch (op.op) {
		case 'start':
			return setStatus(steps, op.id, 'in_progress');
		case 'complete':
			return setStatus(steps, op.id, 'completed');
		case 'cancel':
			return setStatus(steps, op.id, 'cancelled', op.reason);
		case 'block':
			return setStatus(steps, op.id, 'blocked', op.reason);
		case 'add': {
			const step: PlanStep = { id: nextPlanId(steps), content: op.content, status: 'pending' };
			if (op.after_id == null) return [...steps, step];
			const at = steps.findIndex((s) => s.id === op.after_id);
			if (at === -1) return steps; // unknown anchor → no-op
			return [...steps.slice(0, at + 1), step, ...steps.slice(at + 1)];
		}
		default:
			return steps;
	}
}

/// Set a step's status (and reason). Returns the same reference when no step
/// matches `id`, so callers can detect the no-op.
function setStatus(steps: PlanStep[], id: string, status: PlanStatus, reason?: string): PlanStep[] {
	const at = steps.findIndex((s) => s.id === id);
	if (at === -1) return steps;
	const next = [...steps];
	// Keep a prior reason when the new op carries none; a fresh reason overrides.
	next[at] = { ...next[at], status, reason: reason ?? next[at].reason };
	return next;
}

/// Next `add` id: one past the largest numeric id present (matches the backend,
/// so ids stay stable across cancellations).
function nextPlanId(steps: PlanStep[]): string {
	const max = steps.reduce((m, s) => {
		const n = Number(s.id);
		return Number.isInteger(n) && n > m ? n : m;
	}, 0);
	return String(max + 1);
}

/// Index of the most recent plan card, or -1. The newest card is the live plan
/// that non-init ops mutate, and what the UI surfaces as the current plan.
function lastPlanIndex(items: Item[]): number {
	for (let i = items.length - 1; i >= 0; i--) {
		if (items[i].kind === 'plan') return i;
	}
	return -1;
}

function pairResult(
	state: ConversationState,
	callSeq: number,
	output: { content: Array<{ Text: string } | unknown>; is_error: boolean; error_code: string | null },
	failed: boolean
): ConversationState {
	const pos = state.toolSeqs.get(callSeq);
	if (pos === undefined) return state;
	const items = [...state.items];
	const call = items[pos];
	if (call?.kind !== 'tool') return state;
	const text = output.content
		.map((c) => ('Text' in (c as object) ? (c as { Text: string }).Text : '[binary]'))
		.join('');
	items[pos] = { ...call, status: failed || output.is_error ? 'error' : 'done', result: text };
	return { ...state, items };
}

/// Fold one live streaming delta into the conversation state.
///
/// Text and reasoning blocks use **temporal (append-at-end) ordering** to match
/// the TUI: when the model opens a text block at index 0 but fills it after a
/// reasoning block at index 1, the text content still appears after reasoning —
/// matching the user's expected reading order.
///
/// Tool-call blocks keep index-based tracking (via `open`) because tool argument
/// deltas must be matched to the correct tool call.
function applyDelta(
	state: ConversationState,
	ev: GatewayEvent & { type: 'delta' }
): ConversationState {
	const items = [...state.items];
	const open = { ...state.open };

	switch (ev.delta) {
		case 'block_start': {
			const kind = ev.kind === 'reasoning' ? 'reasoning' : ev.kind === 'tool_call' ? 'tool_call' : 'text';
			if (kind === 'tool_call') {
				// Plan is a control tool: show a single streaming placeholder card,
				// not a generic tool block. Its args stream as partial JSON (not
				// foldable mid-stream), so we ignore tool_args for it and let the
				// committed ContentBlock replace the placeholder with the real card.
				if (ev.tool === PLAN_TOOL_NAME) {
					open[ev.index] = items.length;
					items.push({ kind: 'plan', steps: [], streaming: true });
				} else {
					// Tool calls: immediate creation, index-based tracking
					open[ev.index] = items.length;
					items.push({ kind: 'tool', seq: -1, name: ev.tool ?? '', args: '', status: 'running' });
				}
			} else {
				// Text/reasoning: close the previous streaming item of the same kind
				// (so new content for the same kind starts a fresh item at the end),
				// but do NOT create an empty item — defer until content arrives.
				// This avoids premature positioning before the user can see anything.
				for (let i = items.length - 1; i >= 0; i--) {
					const it = items[i];
					if (it.kind === kind && it.streaming) {
						items[i] = { ...it, streaming: false } as Item;
						break;
					}
				}
				// Do not set open[ev.index] — the first content delta will
				// create the item and record its position.
			}
			return { ...state, items, open };
		}
		case 'text': {
			// Extend the existing streaming text item at this index, or create one at the end.
			const pos = open[ev.index];
			const cur = pos !== undefined ? items[pos] : undefined;
			if (cur?.kind === 'text' && cur.streaming) {
				items[pos] = { ...cur, text: cur.text + ev.text };
				return { ...state, items, open };
			}
			// No streaming text at this index. Only create if non-empty
			// (empty deltas are common when a provider opens then abandons a block;
			//  deferring lets a later reasoning block take an earlier visual position).
			if (ev.text) {
				open[ev.index] = items.length;
				items.push({ kind: 'text', text: ev.text, streaming: true });
			}
			return { ...state, items, open };
		}
		case 'reasoning': {
			const pos = open[ev.index];
			const cur = pos !== undefined ? items[pos] : undefined;
			if (cur?.kind === 'reasoning' && cur.streaming) {
				items[pos] = { ...cur, text: cur.text + ev.text };
				return { ...state, items, open };
			}
			if (ev.text) {
				open[ev.index] = items.length;
				items.push({ kind: 'reasoning', text: ev.text, streaming: true });
			}
			return { ...state, items, open };
		}
		case 'tool_args': {
			const pos = open[ev.index];
			const cur = pos !== undefined ? items[pos] : undefined;
			// Plan placeholder: args stream as partial JSON, not foldable until the
			// committed ContentBlock arrives — ignore the stream for it.
			if (cur?.kind === 'plan') return { ...state, items, open };
			if (cur?.kind === 'tool') {
				items[pos] = { ...cur, args: cur.args + ev.json };
				return { ...state, items, open };
			}
			open[ev.index] = items.length;
			items.push({ kind: 'tool', seq: -1, name: '', args: ev.json, status: 'running' });
			return { ...state, items, open };
		}
		default: return assertNever(ev);
	}
}

function push(state: ConversationState, item: Item): ConversationState {
	const items = [...state.items, item];
	// Advance committedEnd so the next RequestStarted truncates past this item.
	// But only when no streaming items exist — if streaming items are present
	// they were created by a race (deltas arrived before this committed event),
	// and including them in committedEnd would prevent truncation from removing them.
	const hasStreaming = items.some((i) => 'streaming' in i && i.streaming);
	return {
		...state,
		items,
		committedEnd: hasStreaming ? state.committedEnd : items.length
	};
}

function assertNever(x: never): never {
	throw new Error(`unhandled variant: ${JSON.stringify(x)}`);
}
