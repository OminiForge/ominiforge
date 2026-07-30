import type { GatewayEvent } from '$lib/types/GatewayEvent';
import type { CoreEvent } from '$lib/types/CoreEvent';
import type { BlockContent } from '$lib/types/BlockContent';
import type { SessionView } from '$lib/types/SessionView';

/** The control tool whose calls drive the todo card. Must match
 *  `TODO_TOOL_NAME` in `src/agent/todo.rs` — todo calls are folded into a
 *  structured todo card instead of rendered as generic tool blocks. */
export const TODO_TOOL_NAME = 'todo';

/** Step lifecycle, mirroring `StepStatus` in `src/agent/todo.rs`. Terminal:
 *  completed/cancelled/blocked; non-terminal: pending/in_progress. */
export type TodoStatus = 'pending' | 'in_progress' | 'completed' | 'cancelled' | 'blocked';

/** One todo step, mirroring `TodoItem` in `src/agent/todo.rs`. `reason` is
 *  carried by cancelled/blocked steps (the why). */
export interface TodoStep {
	id: string;
	content: string;
	status: TodoStatus;
	reason?: string;
}

export type Item =
	/** Every item carries a stable `id` used as the keyed-each identity, so a
	 *  rendering window that prepends history above the viewport doesn't force
	 *  the whole list to re-render (index keys would shift on every prepend).
	 *  Committed items key on the producing event's seq (unique per event,
	 *  stable across replays); transient items (streaming previews, notices,
	 *  errors — none of which come from a committed event) draw from a negative
	 *  counter, a namespace that never collides with a seq. */
	| {
			kind: 'user';
			id: number;
			text: string;
			/** The committed `Turn::Started` event seq — the fork point for
			 *  branching a new session at this turn (`POST /sessions/{id}/fork`).
			 *  Absent on the optimistic local echo (no committed event yet), so
			 *  fork affordances gate on `seq != null`. */
			seq?: number;
			/** True on a LOCALLY inserted bubble whose send was accepted (202) but
			 *  whose `Turn::Started` hasn't folded back yet — normally a blink,
			 *  but with a silently-dead stream it's the only proof the message went
			 *  out. Rendered with a quiet "sending" hint; the authoritative event
			 *  replaces it (see pushOptimisticUser / the Turn.Started fold). */
			pending?: boolean;
	  }
	| { kind: 'text'; id: number; text: string; streaming: boolean }
	| { kind: 'reasoning'; id: number; text: string; streaming: boolean }
	| {
			kind: 'tool';
			id: number;
			seq: number;
			name: string;
			args: string;
			status: 'running' | 'done' | 'error';
			/** One-line summary of the call's args for the collapsed header —
			 *  produced by the tool itself (`Tool::summarize`), rendered verbatim.
			 *  Falls back to truncated raw args when absent (legacy logs). */
			summary?: string;
			result?: string;
			/** Supplementary, debug-only content a tool appended after its primary
			 *  result (currently: LSP diagnostics on read/edit/write —
			 *  `doc/lsp.md` §5). Reaches the model like any other tool
			 *  output; here it's kept out of `result` so it renders only in the
			 *  `RawArgs` debug fold, never mixed into a tool's primary view. */
			diagnostics?: string;
			error_code?: string;
			/** The model-assigned tool-call id — bridges `Permission::Requested`
			 *  (which keys on it) to this card. Present on committed tool items. */
			callId?: string;
			/** A permission `ask` has suspended this call for a human decision
			 *  (`doc/permission.md` §6): the approval controls render inside this
			 *  card (no separate approval card). Cleared by `Permission::Decided`;
			 *  the final status then arrives via `Tool::Completed/Failed`. */
			approvalPending?: boolean;
			/** The backend's UI-only rendering of this call's result
			 *  (`Content::TextView`, `doc/tool-view.md`): the precise diff for
			 *  `edit`/`write`, or the full content for a `write` new file.
			 *  Rendered verbatim by the result component — never rebuilt
			 *  client-side. Absent while running and for tools that produce none. */
			view?: string;
	  }
	/** A todo checklist, folded from `todo` control-tool calls (one card per
	 *  `init`). `streaming` marks a placeholder shown while the call's args are
	 *  still streaming (partial JSON, not yet foldable); it is replaced by the
	 *  real card on commit. See foldTodoOp. */
	| { kind: 'todo'; id: number; steps: TodoStep[]; streaming: boolean }
	/** A one-line activity row in the conversation flow: lighter than a tool
	 *  card, visible unlike the inspect panel. Carries the operations that
	 *  otherwise leave no trace on the timeline — todo ops (the card/dock
	 *  already shows list state; this records what changed, and scrolls away
	 *  with history), hook executions, and runtime reminders (completion gate
	 *  / stuck-step nudges the loop injects mid-turn — not user input, so the
	 *  user must see why the agent "kept going"). `icon` is a render hint;
	 *  `label` is the one-line human-readable summary. */
	| {
			kind: 'activity';
			id: number;
			icon: 'hook' | 'todo' | 'runtime';
			label: string;
			detail?: string;
			/** True while the op this row traces is still streaming/executing.
			 *  A transient row in a live request (negative id) can never be
			 *  replaced by its committed self — commitBlock truncates the whole
			 *  request window — so the row is PUSHED at delta time and flipped
			 *  in place when the committed event lands. */
			streaming?: boolean;
	  }
	| { kind: 'error'; id: number; message: string }
	| { kind: 'notice'; id: number; message: string };

export interface ConversationState {
	items: Item[];
	/** Source of transient (negative) item ids — see `Item.id`. Decremented on
	 *  each allocation. */
	nextId: number;
	/** False while the committed-log replay is still streaming in; the
	 *  gateway's `replay_end` frame flips it true. The view renders NOTHING
	 *  until then (mirroring how the Zed client `await`s a thread's replay
	 *  before handing it to the UI), so history never visibly scrolls past —
	 *  the first paint already shows the full conversation, positioned at the
	 *  tail. A live session with no replay history gets the marker immediately
	 *  after the (empty) replay, so this never latches false. */
	ready: boolean;
	lastSeq?: number;
	lastSettle?: string | null;
	/** block index → items position, current request streaming. Only used for tool_call tracking;
	 *  text/reasoning use temporal (append-at-end) ordering to match the user's
	 *  expected reading order. */
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
	/** callId → items position of a tool card synthesized by a
	 *  `Permission::Requested` that arrived BEFORE its ToolCall ContentBlock
	 *  (out-of-order event delivery). The late ContentBlock completes the
	 *  orphan in place (backfills seq/name/args, registers toolSeqs) instead of
	 *  pushing a duplicate; until then the card's `seq` is the Requested
	 *  event's, which pairResult must never see. */
	orphanTools: Map<string, number>;
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
	return {
		items: [],
		nextId: -1,
		ready: false,
		open: {},
		toolSeqs: new Map(),
		orphanTools: new Map(),
		runtimeModels: new Set()
	};
}

/** Seed the state from the server-folded view (`GET /sessions/{id}/view`):
 *  the items render as-is, and the live stream resumes after `last_seq`.
 *  `ready` starts true — the history is already complete, so there is no
 *  replay boundary to wait for. The fold's live-only bookkeeping (open
 *  blocks, tool pairing maps) starts empty: the server view carries no
 *  in-flight request, and live events rebuild it as they land. */
export function stateFromView(view: SessionView): ConversationState {
	// Rebuild the live pairing bookkeeping for blocks that are still open when
	// the view is taken: a `running` tool card's `Tool::Completed` may commit
	// after the subscribe, and its seq is ≤ last_seq — it will never re-enter
	// through the ContentBlock path, so `pairResult` needs the entry up front
	// or the result is dropped and the card stays running forever.
	const toolSeqs = new Map<number, number>();
	view.items.forEach((item, pos) => {
		if (item.kind === 'tool' && item.status === 'running') toolSeqs.set(item.seq, pos);
	});
	// Every item in the view is committed history, so the live fold resumes
	// with the same bookkeeping a full replay would have at this point:
	// `requestStart`/`commitBase`/`committedEnd` all point past the view, and
	// `requestCommitted` is already true (the view contains committed blocks).
	// Without this, the first live `ContentBlock` would mis-fire the
	// first-commit truncation and slice away the view's items.
	const end = view.items.length;
	return {
		...emptyState(),
		items: view.items,
		lastSeq: view.last_seq ?? undefined,
		ready: true,
		turnRunning: view.turn_running,
		runtimeModels: new Set(view.runtime_models),
		toolSeqs,
		requestStart: end,
		requestCommitted: true,
		commitBase: end,
		committedEnd: end
	};
}

/** Insert a locally-echoed user bubble for a send the gateway ACCEPTED (202)
 *  but whose `Turn::Started` hasn't folded back over the stream yet. The
 *  committed event is normally a blink behind the POST; with a silently-dead
 *  stream it never arrives until a reconnect — without this bubble the user
 *  sees NOTHING for a message the backend is already running, the "did my
 *  message even send?" ambiguity. Matching `Turn::Started` folds remove it
 *  (same text); `subscribe()`'s state reset clears it on session switch. */
export function pushOptimisticUser(state: ConversationState, text: string): ConversationState {
	const pushed = push(state, { kind: 'user', id: state.nextId, text, pending: true });
	return { ...pushed, nextId: state.nextId - 1 };
}

/** Fold many events in one call. Semantically identical to folding them one
 *  `apply` at a time (dedup by seq, ordering, streaming/commit pairing all
 *  unchanged) — this exists so the replay burst is a single state assignment
 *  downstream rather than one invalidation per event. */
export function applyBatch(state: ConversationState, events: GatewayEvent[]): ConversationState {
	let next = state;
	for (const ev of events) next = apply(next, ev);
	return next;
}

export function apply(state: ConversationState, ev: GatewayEvent): ConversationState {
	// Dedup committed events by seq. The gateway subscribes to the live broadcast
	// BEFORE reading its replay log, so an event committed in that gap is
	// delivered twice (once in the replay, once live). Dropping any committed
	// event whose seq was already folded makes that overlap harmless. seqs are
	// monotonic, so `<= lastSeq` is exactly "already seen".
	if (ev.type === 'event' && state.lastSeq !== undefined && Number(ev.seq) <= state.lastSeq) {
		return state;
	}
	switch (ev.type) {
		case 'event':
			return applyCommitted(state, ev);
		case 'delta':
			return applyDelta(state, ev);
		case 'turn_settled':
			return {
				...state,
				lastSettle: ev.incomplete,
				turnRunning: false,
				requestStart: undefined,
				requestCommitted: undefined,
				commitBase: undefined
			};
		case 'compacted':
			return pushTransient(
				{ ...state, turnRunning: false },
				{ kind: 'notice', message: `compacted → ${ev.new_session_id}` }
			);
		case 'notice':
			return pushTransient(
				{ ...state, turnRunning: false },
				{ kind: 'notice', message: ev.message }
			);
		case 'replay_end':
			// The replay burst is done; everything after is live. The view gates
			// its first render on this.
			return { ...state, ready: true };
		// Live-only context occupancy snapshot: handled by the page (STATS panel),
		// not folded into conversation items.
		case 'context_updated':
			return state;
		// Ephemeral prompt hint (permission `ask`): the durable
		// `Permission::Requested` event is what marks the gated tool card
		// approval-pending (see the Permission branch below); this live-only
		// signal just drives the session-list status icon, so it is intentionally
		// not folded here.
		case 'approval_requested':
			return state;
		default:
			return assertNever(ev);
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
			// The authoritative user message: drop any matching optimistic bubble
			// (same text, pending) before pushing the committed one, so a send
			// that outran a dead stream doesn't show its message twice.
			const items = t.Started.input
				? next.items.filter(
						(it) => !(it.kind === 'user' && it.pending && it.text === t.Started.input)
					)
				: next.items;
			const started = { ...next, items, turnRunning: true };
			return t.Started.input
				? push(started, {
						kind: 'user',
						id: Number(core.seq),
						text: t.Started.input,
						seq: Number(core.seq)
					})
				: started;
		}
		if ('Resumed' in t) return { ...next, turnRunning: true };
		if ('Completed' in t || 'Failed' in t || 'Interrupted' in t) {
			// A turn that ends while an `ask` is still pending (cancel / crash /
			// interrupt) leaves an approval prompt that can never resolve — its
			// `Permission::Decided` will never commit because the turn owning the
			// call is gone. Clear those zombie pending flags so a replayed history
			// doesn't show a frozen "等待批准" whose buttons do nothing (the gateway
			// cancel path is racy about writing Decided; this fold is the race-free
			// guarantee).
			const hasPending = next.items.some((it) => it.kind === 'tool' && it.approvalPending);
			if (!hasPending) return { ...next, turnRunning: false };
			const items = next.items.map((it) =>
				it.kind === 'tool' && it.approvalPending ? { ...it, approvalPending: false } : it
			);
			return { ...next, items, turnRunning: false };
		}
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
			return commitBlock(next, Number(core.seq), m.ContentBlock.content, m.ContentBlock.index);
		return next;
	}
	if ('Tool' in payload) {
		const tool = payload.Tool;
		// Todo is a control tool: its Started/Completed pair has no card to
		// pair (calls fold into todo cards instead). An `ops` call surfaces as
		// a one-line activity row so the op itself is visible on the timeline
		// — the card/dock shows list state but not what the agent just did.
		// If the transient row was pushed at delta time, flip THAT row in
		// place: it lives inside the request window commitBlock truncates, so
		// an append here would leave the transient row behind forever. The
		// row's own trace is transient-only (the definitive trace is the
		// committed todo card), so flipping it to settled-not-reidentifiable
		// — not giving it this Started's seq — keeps the next todo call's
		// transient row from being mistaken for it.
		if ('Started' in tool && tool.Started.tool_name === TODO_TOOL_NAME) {
			const label = todoOpLabel(tool.Started.input);
			if (!label) return next;
			const at = transientActivityIndex(next.items, 'todo');
			if (at >= 0) {
				const items = [...next.items];
				items[at] = { ...items[at], streaming: false } as Item;
				return { ...next, items, committedEnd: items.length };
			}
			return push(next, {
				kind: 'activity',
				id: Number(core.seq),
				icon: 'todo',
				label
			});
		}
		if ('Completed' in tool)
			return pairResult(
				next,
				Number(tool.Completed.tool_call_event_id.seq),
				tool.Completed.result,
				false
			);
		if ('Failed' in tool)
			return pairResult(
				next,
				Number(tool.Failed.tool_call_event_id.seq),
				{ content: [{ Text: tool.Failed.error.message }], is_error: true, error_code: null },
				true
			);
		return next;
	}
	if ('Hook' in payload) {
		// Every hook execution lands as a one-line activity row: hooks act on
		// the pipeline invisibly otherwise (the inspect panel is opt-in), and
		// a block or modify the user can't see reads as the agent misbehaving.
		const h = payload.Hook;
		if ('Executed' in h) {
			const e = h.Executed;
			const detail =
				typeof e.outcome === 'object' && 'Blocked' in e.outcome
					? e.outcome.Blocked.reason
					: typeof e.outcome === 'object' && 'Failed' in e.outcome
						? e.outcome.Failed.error
						: undefined;
			const outcome = typeof e.outcome === 'string' ? e.outcome : Object.keys(e.outcome)[0];
			// Flip the transient row in place when it exists (same reasoning as
			// the todo-op row above: appended rows survive commit truncation).
			const at = transientActivityIndex(next.items, 'hook');
			if (at >= 0) {
				const items = [...next.items];
				items[at] = { ...items[at], id: Number(core.seq), streaming: false } as Item;
				return { ...next, items, committedEnd: items.length };
			}
			return push(next, {
				kind: 'activity',
				id: Number(core.seq),
				icon: 'hook',
				label: `${e.hook_name} @ ${e.hook_point} → ${outcome}`,
				detail
			});
		}
		return next;
	}
	if ('Injection' in payload) {
		const inj = payload.Injection;
		// Only Runtime injections surface in the flow: they are the loop nudging
		// itself mid-turn (completion gate / stuck-step reminders), invisible
		// otherwise. Memory/RAG/Hook/ProjectGuidance injections happen at
		// assembly time and are inspect content, not conversation activity.
		if ('ContextInjected' in inj && inj.ContextInjected.source === 'Runtime') {
			const content = inj.ContextInjected.content;
			const at = transientActivityIndex(next.items, 'runtime');
			if (at >= 0) {
				const items = [...next.items];
				items[at] = { ...items[at], id: Number(core.seq), streaming: false } as Item;
				return { ...next, items, committedEnd: items.length };
			}
			return push(next, {
				kind: 'activity',
				id: Number(core.seq),
				icon: 'runtime',
				label: runtimeInjectionLabel(content),
				detail: content
			});
		}
		return next;
	}
	if ('Permission' in payload) {
		const perm = payload.Permission;
		if ('Requested' in perm) {
			// Attach the approval prompt to the gated call's tool card (folded from
			// the earlier `ToolCall` content block, keyed by the same model-assigned
			// call id) — no separate approval card. If that card is somehow missing
			// (the ContentBlock almost always commits first; out-of-order delivery
			// is the exception), synthesize one so the prompt is never lost, and
			// register it as an orphan so the late ContentBlock completes THIS
			// card instead of pushing a duplicate (see commitBlock).
			const r = perm.Requested;
			// The human sees what the call will change via the card's live `view`
			// (stage 2 of the streaming pipeline, already flushed at BlockStop); the
			// gate itself carries no view — approval and view are decoupled
			// (`doc/tool-streaming.md`).
			const pos = next.items.findIndex((it) => it.kind === 'tool' && it.callId === r.call_id);
			if (pos >= 0) {
				const items = [...next.items];
				const it = items[pos];
				if (it.kind === 'tool') items[pos] = { ...it, approvalPending: true };
				return { ...next, items };
			}
			const orphanTools = new Map(next.orphanTools);
			orphanTools.set(r.call_id, next.items.length);
			const pushed = push(next, {
				kind: 'tool',
				id: Number(core.seq),
				seq: Number(core.seq),
				callId: r.call_id,
				name: r.tool_name,
				args: JSON.stringify(r.input),
				status: 'running',
				approvalPending: true
			});
			return { ...pushed, orphanTools };
		}
		if ('Decided' in perm) {
			// The human answered: clear the pending flag and let the paired
			// `Tool::Completed/Failed` drive the card's final status (approved → the
			// call runs; rejected → `denied_by_user`; auto-denied →
			// `denied_no_approval` / `denied_by_policy`).
			const d = perm.Decided;
			const items = next.items.map((it) =>
				it.kind === 'tool' && it.callId === d.call_id ? { ...it, approvalPending: false } : it
			);
			return { ...next, items };
		}
		return next;
	}
	if ('Error' in payload)
		return pushTransient(next, { kind: 'error', message: payload.Error.Raised.message });
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
	content: BlockContent,
	index: number
): ConversationState {
	let items: Item[];
	let commitBase = state.commitBase;
	let truncated = false;

	if (state.requestStart !== undefined && !state.requestCommitted) {
		// First commit: replace all streaming previews with authoritative committed stream.
		items = state.items.slice(0, state.requestStart);
		commitBase = state.requestStart;
		truncated = true;
	} else if (state.requestStart === undefined) {
		// requestStart was cleared (e.g. by turn_settled arriving before the
		// ContentBlock events — a backend event-forwarding race).  Strip any
		// trailing streaming items so committed content replaces them rather
		// than duplicating.
		const firstStreaming = state.items.findIndex((i) => 'streaming' in i && i.streaming);
		if (firstStreaming >= 0) {
			items = state.items.slice(0, firstStreaming);
			commitBase = firstStreaming;
			truncated = true;
		} else {
			items = [...state.items];
		}
	} else {
		items = [...state.items];
	}

	// A truncation above removed this request's streaming previews: every
	// `open` position recorded while they streamed is now stale. Reset it —
	// the preview replacement below looks up `open[index]`, and a stale entry
	// either misses the replacement (duplicate card + stuck running preview)
	// or points at the wrong card entirely.
	const open = truncated ? {} : state.open;

	// Replace the streaming preview for THIS block (keyed on the shared block
	// `index`) instead of appending a duplicate. text/reasoning previews carry
	// `streaming: true` and sit at open[index]; the committed block carries the
	// same index, so completing it in place leaves no stale preview beside the
	// authoritative copy. Skipped after a truncation (open was reset above).
	const previewPos = open[index];
	const preview = previewPos !== undefined ? items[previewPos] : undefined;

	let item: Item;
	if ('Text' in content) {
		if (!content.Text.text.trim())
			return { ...state, open, requestCommitted: true, commitBase, committedEnd: items.length };
		item = { kind: 'text', id: seq, text: content.Text.text, streaming: false };
		if (preview?.kind === 'text' && preview.streaming) {
			items[previewPos] = item;
			return {
				...state,
				items,
				open,
				requestCommitted: true,
				commitBase,
				committedEnd: items.length
			};
		}
		items.push(item);
	} else if ('Reasoning' in content) {
		if (!content.Reasoning.text.trim())
			return { ...state, open, requestCommitted: true, commitBase, committedEnd: items.length };
		item = { kind: 'reasoning', id: seq, text: content.Reasoning.text, streaming: false };
		// A live reasoning preview at this index already sits where the user
		// read it (temporal order): complete it in place rather than splicing a
		// duplicate at commitBase. The commitBase insert below is only for a
		// reasoning block with NO preview (its text sibling committed earlier).
		if (preview?.kind === 'reasoning' && preview.streaming) {
			items[previewPos] = item;
			return {
				...state,
				items,
				open,
				requestCommitted: true,
				commitBase,
				committedEnd: items.length
			};
		}
		// Insert reasoning at commitBase so it appears before any text items
		// that the collector emitted earlier (providers may open text@0 before reasoning@1).
		const insertAt = commitBase ?? items.length;
		items.splice(insertAt, 0, item);
		commitBase = insertAt + 1;
		return {
			...state,
			items,
			// The splice shifted every item at/after insertAt one position up:
			// the position books must shift with it or pairResult/preview
			// replacement write the wrong card.
			...shiftPositions(state, insertAt),
			requestCommitted: true,
			commitBase,
			committedEnd: items.length
		};
	} else {
		// Todo is a control tool: fold its op into a todo card instead of
		// rendering a generic tool block. The card lives where `init` lands and
		// later ops mutate it in place (mirrors the backend's single authoritative
		// todo, but each `init` starts a fresh card so turn history is preserved).
		if (content.ToolCall.name === TODO_TOOL_NAME) {
			// A truncation above sliced away this call's transient activity row
			// (it sat at/after requestStart with no committed seq). Re-push it
			// as the settled trace row — the definitive trace is the committed
			// todo card, so the row carries no id link and the paired
			// Tool::Started leaves it alone (it only flips streaming rows).
			const lostRow = truncated && state.items.some((i) => i.kind === 'activity' && i.streaming);
			items = foldTodoOp(items, seq, content.ToolCall.arguments);
			if (lostRow) {
				items.push({
					kind: 'activity',
					id: state.nextId,
					icon: 'todo',
					label: 'Updated todo list',
					streaming: false
				});
			}
			return {
				...state,
				items,
				open,
				requestCommitted: true,
				commitBase,
				committedEnd: items.length,
				nextId: lostRow ? state.nextId - 1 : state.nextId
			};
		}
		const toolSeqs = new Map(state.toolSeqs);
		let orphanTools = state.orphanTools;
		const orphanAt = orphanTools.get(content.ToolCall.id);
		if (orphanAt !== undefined) {
			// A Permission::Requested synthesized this call's card before the
			// ToolCall committed (out-of-order delivery). Complete the orphan —
			// pushing a fresh card would duplicate it, and only registering the
			// REAL ToolCall seq lets the paired Tool::Completed/Failed find the
			// card (the orphan's provisional seq is the Requested event's).
			orphanTools = new Map(orphanTools);
			orphanTools.delete(content.ToolCall.id);
			// The recorded index may be stale: the first-commit truncation above
			// can slice away items appended after requestStart, or shift them.
			// Re-validate, then fall back to a fresh lookup by callId.
			const isOrphanAt = (p: number) => {
				const it = items[p];
				return it !== undefined && it.kind === 'tool' && it.callId === content.ToolCall.id;
			};
			const pos = isOrphanAt(orphanAt)
				? orphanAt
				: items.findIndex((it) => it.kind === 'tool' && it.callId === content.ToolCall.id);
			if (pos >= 0) {
				const cur = items[pos];
				if (cur.kind === 'tool') {
					toolSeqs.set(seq, pos);
					// Spread keeps approvalPending: the ask is still outstanding
					// (or was already cleared by a Decided that landed on the orphan).
					// Re-key on the real ToolCall seq: the orphan was keyed on the
					// Requested event's seq, which must not linger as a DOM identity.
					items[pos] = {
						...cur,
						id: seq,
						seq,
						name: content.ToolCall.name,
						args: content.ToolCall.arguments,
						summary: content.ToolCall.summary ?? undefined
					};
					return {
						...state,
						items,
						open,
						toolSeqs,
						orphanTools,
						requestCommitted: true,
						commitBase,
						committedEnd: items.length
					};
				}
			}
			// The orphan itself was truncated away above: fall through and push
			// a fresh card, but keep the outstanding ask on it — dropping
			// approvalPending here would disarm a prompt the human may be
			// answering right now. (If a severely-reordered Decided already
			// landed, the flag is a zombie that the turn-end fold clears.)
			toolSeqs.set(seq, items.length);
			items.push({
				kind: 'tool',
				id: seq,
				seq,
				callId: content.ToolCall.id,
				name: content.ToolCall.name,
				args: content.ToolCall.arguments,
				status: 'running',
				summary: content.ToolCall.summary ?? undefined,
				approvalPending: true
			});
			return {
				...state,
				items,
				open,
				toolSeqs,
				orphanTools,
				requestCommitted: true,
				commitBase,
				committedEnd: items.length
			};
		}
		// Replace the streaming preview for THIS block instead of appending a
		// duplicate (previewPos/preview computed above). Only done when the card
		// at open[index] really is the preview (seq=-1): after a truncation
		// `open` was reset above, so a stale position can never clobber an
		// already-committed card.
		if (preview?.kind === 'tool' && preview.seq === -1) {
			toolSeqs.set(seq, previewPos);
			items[previewPos] = {
				kind: 'tool',
				id: seq,
				seq,
				callId: content.ToolCall.id,
				name: content.ToolCall.name,
				args: content.ToolCall.arguments,
				status: 'running',
				summary: content.ToolCall.summary ?? undefined
			};
			return {
				...state,
				items,
				open,
				toolSeqs,
				requestCommitted: true,
				commitBase,
				committedEnd: items.length
			};
		}
		toolSeqs.set(seq, items.length);
		item = {
			kind: 'tool',
			id: seq,
			seq,
			callId: content.ToolCall.id,
			name: content.ToolCall.name,
			args: content.ToolCall.arguments,
			status: 'running',
			summary: content.ToolCall.summary ?? undefined
		};
		items.push(item);
		return {
			...state,
			items,
			open,
			toolSeqs,
			requestCommitted: true,
			commitBase,
			committedEnd: items.length
		};
	}

	return { ...state, items, open, requestCommitted: true, commitBase, committedEnd: items.length };
}

/// Shift every recorded items position at/after `insertAt` up by one, after a
/// mid-list insertion (the reasoning commitBase splice). Returns fresh maps —
/// callers spread the result, so an untouched book keeps its identity.
function shiftPositions(
	state: ConversationState,
	insertAt: number
): Pick<ConversationState, 'toolSeqs' | 'orphanTools' | 'open'> {
	const shift = (pos: number) => (pos >= insertAt ? pos + 1 : pos);
	const toolSeqs = new Map<number, number>();
	for (const [seq, pos] of state.toolSeqs) toolSeqs.set(seq, shift(pos));
	const orphanTools = new Map<string, number>();
	for (const [id, pos] of state.orphanTools) orphanTools.set(id, shift(pos));
	const open: Record<number, number> = {};
	for (const [idx, pos] of Object.entries(state.open)) open[Number(idx)] = shift(pos);
	return { toolSeqs, orphanTools, open };
}

/// Decoded `todo` call, mirroring `TodoOp`/`LeafOp` in `src/agent/todo.rs`.
/// Two shapes only: `init` establishes the list, `ops` mutates it (a single
/// change is a one-element array). Only the fields each op needs are read;
/// the rest are ignored, matching serde's tolerance on the wire.
type TodoCall = { op: 'init'; steps?: Array<{ content: string }> } | { ops?: LeafOp[] };

type LeafOp =
	| { op: 'start'; id: string }
	| { op: 'complete'; id: string }
	| { op: 'cancel'; id: string; reason?: string }
	| { op: 'block'; id: string; reason?: string }
	| { op: 'add'; content: string; after_id?: string };

/** Push a transient item, allocating its negative id from the state counter
 *  (see `Item.id`). Every push site that does not carry a committed event seq
 *  (notices, errors) goes through here. */
function pushTransient(
	state: ConversationState,
	item: { kind: 'error' | 'notice'; message: string }
): ConversationState {
	const pushed = push(state, { ...item, id: state.nextId });
	return { ...pushed, nextId: state.nextId - 1 };
}

/// Find the newest transient activity row of the given icon (a row pushed at
/// delta time for a live op). Returns its items index, or -1.
function transientActivityIndex(items: Item[], icon: 'hook' | 'todo' | 'runtime'): number {
	for (let i = items.length - 1; i >= 0; i--) {
		const it = items[i];
		if (it.kind === 'activity' && it.icon === icon && it.streaming) return i;
	}
	return -1;
}

/// One-line label for a runtime-injected reminder (completion gate /
/// stuck-step). The content is a `<reminder>…</reminder>` block (`doc/todo.md`
/// §8); the label is its first line with the tags stripped. The prefix names
/// the audience — the reminder addresses the MODEL, not the user — so the
/// user reads the row as the loop nudging its model, not as a message to them.
function runtimeInjectionLabel(content: string): string {
	const stripped = content.replace(/<\/?reminder>/g, '').trim();
	const firstLine = stripped.split('\n', 1)[0] ?? '';
	return `Model reminder: ${firstLine}`;
}

/// One-line label for a committed todo `ops` call (from its `Tool::Started`
/// input), e.g. `Completed step 2 · Blocked step 3: waiting for API key`. Returns
/// `null` for `init` (the todo card itself marks it) and for malformed input
/// — the fold never throws on model-authored JSON.
function todoOpLabel(input: unknown): string | null {
	if (!input || typeof input !== 'object') return null;
	const ops = (input as { ops?: unknown }).ops;
	if (!Array.isArray(ops)) return null;
	if (ops.length === 0) return 'Updated todo list (no changes)';
	const parts: string[] = [];
	for (const raw of ops) {
		if (!raw || typeof raw !== 'object') return null;
		const op = raw as LeafOp;
		switch (op.op) {
			case 'start':
				parts.push(`Started step ${op.id}`);
				break;
			case 'complete':
				parts.push(`Completed step ${op.id}`);
				break;
			case 'cancel':
				parts.push(`Cancelled step ${op.id}${op.reason ? `: ${op.reason}` : ''}`);
				break;
			case 'block':
				parts.push(`Blocked step ${op.id}${op.reason ? `: ${op.reason}` : ''}`);
				break;
			case 'add':
				parts.push(`Added step: ${op.content}`);
				break;
			default:
				return null;
		}
	}
	return parts.join(' · ');
}

/// Fold one committed `todo` tool call into the items list.
///
/// Strategy mirrors the backend (`src/agent/todo.rs`): `init` replaces the list
/// with a fresh card; every op in `ops` mutates the *latest* todo card in
/// place, in array order. The frontend keeps one card per `init` (not a single
/// global list) so the conversation preserves the todo list of each turn as history
/// — the newest card is always the live one that subsequent ops target.
///
/// Robustness: the args are authoritative committed JSON, but a malformed op or
/// a mutation with no card to target is ignored (the items list is returned
/// unchanged), mirroring the backend's benign `is_error` handling — the model
/// corrects itself next round and we never throw mid-fold.
function foldTodoOp(items: Item[], id: number, args: string): Item[] {
	let call: TodoCall;
	try {
		call = JSON.parse(args) as TodoCall;
	} catch {
		return items;
	}
	if (!call || typeof call !== 'object') return items;

	if ('op' in call && call.op === 'init') {
		const steps: TodoStep[] = (call.steps ?? []).map((s, i) => ({
			id: String(i + 1),
			content: s.content,
			status: 'pending'
		}));
		const card = { kind: 'todo', id, steps, streaming: false } as Item;
		return [...items, card];
	}

	if (!('ops' in call) || !Array.isArray(call.ops)) return items;

	// Mutate the latest todo card. No card → benign no-op (model misused todo).
	const pos = lastTodoIndex(items);
	if (pos === -1) return items;
	const card = items[pos];
	if (card.kind !== 'todo') return items;
	let steps = card.steps;
	for (const op of call.ops) {
		steps = applyLeafOp(steps, op);
	}
	if (steps === card.steps) return items; // unchanged (unknown id / anchor)
	const next = [...items];
	next[pos] = { ...card, steps };
	return next;
}

/// Apply one leaf op to a step list, returning a new list (or the same
/// reference unchanged when the target id/anchor is absent — a benign no-op
/// matching the backend's `TodoError` → `is_error` path).
function applyLeafOp(steps: TodoStep[], op: LeafOp): TodoStep[] {
	if (!op || typeof op !== 'object') return steps;
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
			const step: TodoStep = { id: nextTodoId(steps), content: op.content, status: 'pending' };
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
function setStatus(steps: TodoStep[], id: string, status: TodoStatus, reason?: string): TodoStep[] {
	const at = steps.findIndex((s) => s.id === id);
	if (at === -1) return steps;
	const next = [...steps];
	// Keep a prior reason when the new op carries none; a fresh reason overrides.
	next[at] = { ...next[at], status, reason: reason ?? next[at].reason };
	return next;
}

/// Next `add` id: one past the largest numeric id present (matches the backend,
/// so ids stay stable across cancellations).
function nextTodoId(steps: TodoStep[]): string {
	const max = steps.reduce((m, s) => {
		const n = Number(s.id);
		return Number.isInteger(n) && n > m ? n : m;
	}, 0);
	return String(max + 1);
}

/// Index of the most recent todo card, or -1. The newest card is the live list
/// that non-init ops mutate, and what the UI surfaces as the current todo list.
function lastTodoIndex(items: Item[]): number {
	for (let i = items.length - 1; i >= 0; i--) {
		if (items[i].kind === 'todo') return i;
	}
	return -1;
}

function pairResult(
	state: ConversationState,
	callSeq: number,
	output: {
		content: Array<{ Text: string } | unknown>;
		is_error: boolean;
		error_code: string | null;
	},
	failed: boolean
): ConversationState {
	const pos = state.toolSeqs.get(callSeq);
	if (pos === undefined) return state;
	const items = [...state.items];
	const call = items[pos];
	if (call?.kind !== 'tool') return state;
	// Content blocks partition by role (`doc/tool-view.md` §3): `Text` is the
	// model-facing result; `TextView` (audience "ui") is the backend's UI-only
	// structured view (a JSON envelope `{ kind, ... }` — the diff/code/terminal
	// view this card dispatches on `kind` and renders verbatim); trailing `Text`
	// on the built-in file tools is the debug-only LSP diagnostics block
	// (`doc/lsp.md` §5).
	//
	// `content[0]` (Text) is the tool's primary result — what `result` means
	// everywhere else in the UI (ReadResult parses it as the file body). Only
	// the built-in file tools append supplementary content after it; every
	// other tool (notably MCP tools that legitimately return text+image or
	// multi-text) keeps all entries joined into the primary result — splitting
	// them would silently hide real content. The diagnostics split is
	// therefore gated on the tool name, not on positional index.
	const isAssistTool = call.name === 'read' || call.name === 'write' || call.name === 'edit';
	const texts: string[] = [];
	let view: string | undefined;
	for (const c of output.content) {
		if ('TextView' in (c as object)) {
			const tv = (c as { TextView: { text: string; audience: string } }).TextView;
			if (tv.audience === 'ui') view = tv.text;
		} else if ('Text' in (c as object)) {
			texts.push((c as { Text: string }).Text);
		} else {
			texts.push('[binary]');
		}
	}
	let text: string;
	let diagnostics: string | undefined;
	if (isAssistTool && texts.length > 1) {
		text = texts[0] ?? '';
		diagnostics = texts.slice(1).join('');
	} else {
		text = texts.join('');
		diagnostics = undefined;
	}
	items[pos] = {
		...call,
		status: failed || output.is_error ? 'error' : 'done',
		result: text,
		diagnostics,
		// The executed view replaces the stage-2 streaming snapshot (same
		// envelope, now complete). Absent on a no-op edit or a failure — the
		// card then keeps whatever stage-2 view it had, or none.
		view,
		error_code: output.error_code ?? undefined
	};

	return { ...state, items };
}

/// Fold one live streaming delta into the conversation state.
///
/// Text and reasoning blocks use **temporal (append-at-end) ordering**: when
/// the model opens a text block at index 0 but fills it after a reasoning
/// block at index 1, the text content still appears after reasoning — matching
/// the user's expected reading order.
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
			const kind =
				ev.kind === 'reasoning' ? 'reasoning' : ev.kind === 'tool_call' ? 'tool_call' : 'text';
			if (kind === 'tool_call') {
				// Todo is a control tool: push a transient activity row ("updating
				// the todo list…") instead of a placeholder card — the committed
				// Tool::Started flips this row in place with the real op label.
				// tool_args deltas stream partial JSON (not foldable mid-stream),
				// so they are ignored for todo.
				if (ev.tool === TODO_TOOL_NAME) {
					open[ev.index] = items.length;
					items.push({
						kind: 'activity',
						id: state.nextId,
						icon: 'todo',
						label: 'Updating todo list…',
						streaming: true
					});
				} else {
					// Tool calls: immediate creation, index-based tracking
					open[ev.index] = items.length;
					items.push({
						kind: 'tool',
						id: state.nextId,
						seq: -1,
						name: ev.tool ?? '',
						args: '',
						status: 'running'
					});
				}
				return { ...state, items, open, nextId: state.nextId - 1 };
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
				items.push({ kind: 'text', id: state.nextId, text: ev.text, streaming: true });
				return { ...state, items, open, nextId: state.nextId - 1 };
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
				items.push({ kind: 'reasoning', id: state.nextId, text: ev.text, streaming: true });
				return { ...state, items, open, nextId: state.nextId - 1 };
			}
			return { ...state, items, open };
		}
		// A render-ready snapshot of a tool call's live state (stage 2 of the
		// streaming tool-call pipeline, `doc/tool-streaming.md`): a presenter's
		// growing args view (`write`/`edit`) or a result-streaming tool's output
		// (`shell`). Written into the SAME `view` field the settled TextView lands
		// in, so the card renders stage 2 and stage 3 with one code path — the
		// snapshot just grows until the settled view replaces it. Snapshots are
		// self-contained (never a delta), so a stale one is simply overwritten.
		// Raw args are no longer streamed live (the old `tool_args` path is gone);
		// the full args land at stage 3 for the debug fold.
		case 'tool_progress': {
			const pos = open[ev.index];
			const cur = pos !== undefined ? items[pos] : undefined;
			if (cur?.kind === 'tool') {
				items[pos] = { ...cur, view: ev.view };
				return { ...state, items, open };
			}
			// No card at this index (e.g. a snapshot raced ahead of its
			// block_start): drop it — the next snapshot or the settled view
			// repaints. Self-contained snapshots make a dropped frame harmless.
			return { ...state, items, open };
		}
		// The model request hit a transient failure (network drop, 429/5xx engine
		// overload) and is being retried: surface it as a transient notice so the
		// user sees "retrying…" instead of a silent pause during the backoff.
		case 'retrying': {
			const seconds = Math.round(Number(ev.delay_ms) / 1000);
			return pushTransient(state, {
				kind: 'notice',
				message: `model request failed (${ev.error}); retrying in ${seconds}s (attempt ${ev.attempt}/${ev.max_retries})`
			});
		}
		default:
			return assertNever(ev);
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
