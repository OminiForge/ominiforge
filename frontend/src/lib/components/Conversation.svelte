<script lang="ts">
	import { onDestroy, onMount, tick } from 'svelte';
	import { browser } from '$app/environment';
	import { goto } from '$app/navigation';
	import { fade, fly, scale } from 'svelte/transition';
	import { client } from '$lib/client';
	import type { ConnectionState, EventSubscription } from '$lib/client-core';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
	import type { Message } from '$lib/types/Message';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import type { ProfileSummary } from '$lib/types/ProfileSummary';
	import type { ModelSummary } from '$lib/types/ModelSummary';
	import {
		applyBatch,
		emptyState,
		pushOptimisticUser,
		stateFromView,
		type ConversationState,
		type Item,
		type TodoStep
	} from '$lib/conversation';
	import type { GatewayEvent } from '$lib/types/GatewayEvent';
	import Skeleton from '$lib/components/Skeleton.svelte';
	import PickerSelect from '$lib/components/PickerSelect.svelte';
	import { type SelectOption } from '$lib/components/ModelSelect.svelte';
	import ConversationItem from '$lib/components/ConversationItem.svelte';
	import TodoCard from '$lib/components/TodoCard.svelte';
	import DetailRail from '$lib/components/DetailRail.svelte';
	import type { ApprovalScope } from '$lib/types/ApprovalScope';
	import { renderMarkdown, renderUserMarkdown } from '$lib/markdown';
	import { groupInspectEvents, type RawEvent } from '$lib/inspect';
	import { markSeen, notifySessionEvent } from '$lib/status.svelte';
	import { loadQueue, saveQueue, enqueue, removeFromQueue, type QueuedMessage } from '$lib/queue';
	import { activeTick, jumpTarget } from '$lib/minimap';
	import { setPendingFork, takePendingFork, type PendingFork } from '$lib/fork';
	import { rise, pop, fadeIn } from '$lib/motion';

	/** Props: the workspace this conversation lives under (its path-derived id,
	 *  used to build session URLs) and the session id to show (`'new'` for a
	 *  draft). Both come from the route params via the thin page wrappers, so a
	 *  sidebar navigation (URL change) re-seeds the view. */
	interface Props {
		workspaceId: string;
		routeSessionId: string;
	}
	let { workspaceId, routeSessionId }: Props = $props();

	/** Sentinel id for a not-yet-created (draft) session. Opening a draft shows an
	 *  empty conversation; the real session is created lazily on the first send, so
	 *  merely opening a draft never litters the store with empty sessions. The
	 *  backend never mints `new` as a real id, so it can't clash. */
	const DRAFT_ID = 'new';

	/** When the user is within this many pixels from the bottom we consider
	 *  them "at the bottom" and auto-scroll on new content. This tolerance
	 *  avoids missing the trigger due to sub-pixel rounding or small
	 *  layout shifts. */
	const SCROLL_BOTTOM_THRESHOLD = 80;

	/** How long after a successful send we tolerate a silent event stream
	 *  before forcing a reconnect. The gateway commits `Turn::Started` right
	 *  after accepting the message, so a healthy stream shows it within a
	 *  second or two; 10s leaves generous room for server load without making
	 *  the user stare at a dead view. */
	const SEND_LIVENESS_MS = 10_000;

	let convo = $state<ConversationState>(emptyState());
	/** Raw committed events, kept alongside the folded `items` so inspect mode
	 *  can render the full timeline (including history that predates the current
	 *  subscription). Only `type: 'event'` entries are stored — deltas are
	 *  transient and not replayed.
	 *
	 *  NOT reactive: during the replay burst thousands of events arrive in a
	 *  tight loop, and a `$state` array would invalidate + re-render on every
	 *  one (and a `[...rawLog, ev]` spread is an O(n²) copy). Inspect mode reads
	 *  this imperatively; `inspectTick` is bumped once per batch to tell the
	 *  timeline to re-read. */
	// `$state.raw`: the array reference is reactive (session switch replaces
	// it), but push() does NOT trigger invalidation — exactly what the replay
	// burst needs. The inspect timeline re-reads via `inspectTick`.
	let rawLog = $state.raw<RawEvent[]>([]);
	/** Bumped (cheap, batched) when rawLog grows, so the inspect timeline
	 *  re-reads it without rawLog itself being reactive. */
	let inspectTick = $state(0);
	let inspectRaf = 0;
	function scheduleInspectTick() {
		if (!browser) return;
		cancelAnimationFrame(inspectRaf);
		inspectRaf = requestAnimationFrame(() => inspectTick++);
	}
	/** Inspect mode is the "timeline" tab of the right detail rail: instead of a
	 *  floating overlay, the rail hosts Info and Inspect as switchable tabs. */
	let inspectMode = $state(false);
	/** Tab switching only flips which pane shows; it never opens/closes the
	 *  rail itself (that's the topbar rail toggle's job). */
	function setInspect(on: boolean) {
		inspectMode = on;
	}
	/** Inspect timeline order: false = chronological (oldest first), true =
	 *  newest first (like a log tail). Display-only — rawLog itself stays in
	 *  commit order. */
	let inspectReversed = $state(false);
	/** The events in display order. rawLog is `$state.raw` (push doesn't
	 *  invalidate), so this derives from inspectTick instead — it's bumped once
	 *  per batch, which is exactly when the timeline re-reads. The spread +
	 *  reverse is one O(n) copy per batch, not per event. */
	const inspectEvents = $derived.by(() => {
		void inspectTick;
		const list = [...rawLog];
		if (inspectReversed) list.reverse();
		return list;
	});
	/** The timeline rows: multi-phase actions (model requests, tool calls)
	 *  folded into one expandable group row, everything else a single row.
	 *  Grouping runs over the display-ordered list, so a reversed timeline
	 *  shows the group's LAST phase as its row position. */
	const inspectRows = $derived(groupInspectEvents(inspectEvents));
	let input = $state('');
	let sending = $state(false);
	// Messages the user sent while a turn was running: held here (and mirrored to
	// localStorage per session) as pending chips instead of hitting the gateway,
	// which would defer them invisibly. Flushed one-at-a-time on `turn_settled`.
	let queued = $state<QueuedMessage[]>([]);
	let error = $state<string | null>(null);
	/** Event-stream link state, from the transport's onConnection callback.
	 *  `connecting` = initial attach or a reconnect after a drop — shown as a
	 *  quiet banner instead of a scary error bar; `connected` clears it. The
	 *  stall watchdog in the transport is what flips a silently-dead stream
	 *  back to `connecting` (previously the UI just froze until a refresh). */
	let connection = $state<ConnectionState>('connecting');
	// Draft-only session config: profile / model override / workspace, chosen
	// before the first send. Populated from the gateway when a draft opens; the
	// real session is created with these on first send (and they're read-only
	// thereafter — a session's config is immutable, doc/profile.md §5).
	let profiles = $state<ProfileSummary[]>([]);
	let models = $state<ModelSummary[]>([]);
	let selProfile = $state('');
	// Model override as `provider/model_id`; empty = use the profile default.
	let selModel = $state('');
	// Per-turn reasoning-effort tier (a raw string the current model declares);
	// empty = the session's configured default. Applies to the next send only.
	let selEffort = $state('');
	// Which picker popover is open (only one at a time); `null` = all closed.
	// The active session id is exactly the route param: the draft (`'new'`), or a
	// real id under `.../sessions/[id]`. Every transition — draft first-send,
	// reconfigure, compaction, sidebar click, back/forward — is a real navigation
	// (goto), so the id only ever changes via the route. Deriving it (rather than
	// mirroring it into local state) means the main effect below re-subscribes on
	// every route change with no chance of local state fighting the prop.
	const sessionId = $derived(routeSessionId);
	let meta = $state<SessionMeta | null>(null);
	// An archived session (`doc/session-storage.md` §9) is read-only: its history
	// replays from the log, but no turn can ever run on it again — so the input
	// is disabled rather than letting a send fail with a 410 after the fact. The
	// id guard keeps a previous session's meta from leaking its archived state
	// into the session we're switching to.
	const isArchived = $derived(meta?.id === sessionId ? (meta?.archived ?? false) : false);
	// Inherited context for a branched session (origin != new): the parent's
	// conversation the fork/compaction/reconfiguration was seeded with, loaded
	// from `context_snapshot.json` via getSnapshot and rendered as dimmed history
	// above the live turns so the user sees what came before (issue: a fork must
	// not look like it started from nothing). Empty = none/failed/`new` session.
	let inherited = $state<InheritedItem[]>([]);
	// Config-layer provider/model for the RUNTIME panel; null until loaded or on
	// a failed lookup. Local to this page now (the panel moved off the global
	// sidebar into this page's right detail column).
	let runtime = $state<RuntimeInfo | null>(null);
	// Folded summary snapshot for the STATS panel. Best-effort: refreshed on load
	// and whenever a turn settles, so the metrics track the live conversation
	// without rebuilding them from the event fold.
	let summary = $state<SessionSummary | null>(null);
	// Live context-window occupancy, from the per-round `context_updated` event
	// (ephemeral: not replayed). The detail rail falls back to the summary's
	// persisted `context_tokens` when this is null (page reload, idle session),
	// so the gauge still reads after a refresh. `tokens` is the running
	// estimate; `window` the model's full context window (0 = unknown);
	// `threshold` the compaction fraction (drawn as a gauge tick — the gauge
	// is tokens/window, NOT tokens/effective_limit). Reset on session switch.
	let context = $state<{ tokens: number; window: number; threshold: number } | null>(null);
	// Debounce handle for per-request STATS refresh (Q2): a long turn fires many
	// RequestCompleted events; coalesce them so we don't replay the log per event.
	let summaryDebounce: ReturnType<typeof setTimeout> | undefined;
	/** Post-send liveness timer: cleared on each send and on destroy. */
	let livenessTimer: ReturnType<typeof setTimeout> | undefined;
	let sub: EventSubscription | undefined;
	let streamEl = $state<HTMLElement | null>(null);
	/** True while the server-folded view is being fetched (session opening). */
	let loading = $state(false);
	/** Guards async view-fetch callbacks against firing after a session switch. */
	let subscribeGen = 0;

	// ── Event intake: coalesce the replay burst into one fold per frame ──
	//
	// The gateway replays the full committed log over SSE on subscribe, one
	// frame per event. Folding each event straight into `convo` meant one state
	// assignment (and one Svelte invalidation) per event — the "click a session,
	// wait seconds" path this redesign removes. Instead every event (replay or
	// live) is pushed into this plain buffer and folded once per animation
	// frame; live events ride the same path, so no ordering can invert.
	//
	// The view renders NOTHING until the gateway's `replay_end` frame folds in
	// (state.ready) — the same "await replay, then hand to the UI" sequencing
	// the Zed client uses — so the first paint already shows the full
	// conversation, positioned at the tail. No incremental mounting, no scroll
	// anchoring: after `ready`, content only ever grows at the tail.
	let eventBuffer: GatewayEvent[] = [];
	let flushRaf = 0;
	function scheduleFlush() {
		if (!browser || flushRaf) return;
		flushRaf = requestAnimationFrame(flushEvents);
	}
	function flushEvents() {
		if (flushRaf) {
			cancelAnimationFrame(flushRaf);
			flushRaf = 0;
		}
		if (eventBuffer.length === 0) return;
		const batch = eventBuffer;
		eventBuffer = [];
		convo = applyBatch(convo, batch);
		if (shouldAutoScroll) scrollTailSoon();
	}
	function scrollTailSoon() {
		requestAnimationFrame(() => {
			streamEl?.scrollTo({ top: streamEl.scrollHeight, behavior: 'smooth' });
		});
	}

	/** Items that folded from the replay burst render without the rise
	 *  animation (they are history, appearing with the first paint); only
	 *  genuinely live items animate in. Set when the replay boundary folds in. */
	let replayedCount = $state(0);

	/** The replay boundary (`replay_end`) has folded in: reveal the
	 *  conversation and land at the tail instantly (no smooth animation —
	 *  opening a session should BE at the bottom, never scroll there). */
	function presentReplayedHistory() {
		replayedCount = convo.items.length;
		tick().then(() => {
			if (!streamEl) return;
			streamEl.scrollTop = streamEl.scrollHeight;
			lastScrollTop = streamEl.scrollTop;
			shouldAutoScroll = true;
		});
	}
	// Whether the user is scrolled to (or near) the bottom – controls auto-scroll.
	let shouldAutoScroll = $state(true);
	// User-message minimap: one tick per user turn on the scroll rail, for
	// jump-to-message (click or Ctrl+↑/↓). Positions are FRACTIONS of the scroll
	// content (0..1), measured from the DOM because item heights vary with content
	// and streaming; re-measured on resize + item changes. `scrollFrac` is the
	// viewport's own position, used to highlight the tick the user is currently at.
	let ticks = $state<{ index: number; top: number; preview: string }[]>([]);
	let scrollFrac = $state(0);
	let measureRaf = 0;

	/** Re-measure tick positions on the next frame (coalesces bursts of resize /
	 *  item-change triggers into one layout read). */
	function scheduleMeasure() {
		if (!browser) return;
		cancelAnimationFrame(measureRaf);
		measureRaf = requestAnimationFrame(measureTicks);
	}

	/** Read each user message's position within the scroll content. Uses
	 *  bounding-rect math (not offsetTop) so it's correct regardless of the
	 *  offset-parent chain. */
	function measureTicks() {
		const el = streamEl;
		if (!el) {
			ticks = [];
			return;
		}
		const total = el.scrollHeight;
		if (total <= 0) {
			ticks = [];
			return;
		}
		const elTop = el.getBoundingClientRect().top;
		const nodes = el.querySelectorAll<HTMLElement>('[data-user-anchor]');
		const next: { index: number; top: number; preview: string }[] = [];
		nodes.forEach((node) => {
			const rel = node.getBoundingClientRect().top - elTop + el.scrollTop;
			next.push({
				index: Number(node.dataset.userAnchor),
				top: rel / total,
				preview: (node.textContent ?? '').trim().split('\n')[0].slice(0, 60)
			});
		});
		ticks = next;
	}

	/** The tick the viewport top currently sits at/after, for the active-tick
	 *  highlight. Delegates to the pure `activeTick` geometry (unit-tested). */
	const activeTickIndex = $derived.by(() => activeTick(ticks, scrollFrac));

	/** Scroll a specific user message (by its items index) to the top of the view. */
	function scrollToUserMessage(index: number) {
		const el = streamEl;
		if (!el) return;
		const node = el.querySelector<HTMLElement>(`[data-user-anchor="${index}"]`);
		if (!node) return;
		const top = node.getBoundingClientRect().top - el.getBoundingClientRect().top + el.scrollTop;
		el.scrollTo({ top, behavior: 'smooth' });
	}

	/** Inspect → conversation jump: user items key on their Turn::Started seq,
	 *  tool items on the model's ToolCall block seq, so most events land on the
	 *  item at-or-just-before their seq. Internal events that precede ANY visible
	 *  message (Session.Created, early injections/hooks) have no such anchor —
	 *  they fall forward to the FIRST item after them, i.e. the message whose
	 *  turn they belong to. Either way every row jumps somewhere; the rail stays
	 *  on the inspect tab so the user can click several events in a row. */
	function scrollToSeq(seq: number) {
		const el = streamEl;
		if (!el) return;
		let anchor: number | null = null;
		for (let i = 0; i < convo.items.length; i++) {
			const it = convo.items[i];
			if ((it.kind === 'user' || it.kind === 'tool') && it.seq != null && it.seq <= seq) anchor = i;
		}
		if (anchor == null) {
			// No visible message at/before this event: take the first one after it.
			for (let i = 0; i < convo.items.length; i++) {
				const it = convo.items[i];
				if ((it.kind === 'user' || it.kind === 'tool') && it.seq != null && it.seq > seq) {
					anchor = i;
					break;
				}
			}
		}
		if (anchor == null) return;
		const node = el.querySelector<HTMLElement>(`[data-item-anchor="${anchor}"]`);
		if (!node) return;
		const top = node.getBoundingClientRect().top - el.getBoundingClientRect().top + el.scrollTop;
		el.scrollTo({ top, behavior: 'smooth' });
	}

	/** Jump to the previous/next user message relative to the current scroll
	 *  position. Target geometry is the pure `jumpTarget` (unit-tested); this just
	 *  applies it to the live element. Downward past the last message continues to
	 *  the very bottom, so a final Ctrl+↓ lands on the latest content (agent reply,
	 *  tools) rather than dead-ending on the last user turn. */
	function jumpUserMessage(dir: 1 | -1) {
		const el = streamEl;
		if (!el) return;
		const target = jumpTarget(ticks, el.scrollTop, el.scrollHeight, dir);
		if (target !== null) {
			el.scrollTo({ top: target, behavior: 'smooth' });
		} else if (dir === 1) {
			// No next message below: fall through to the bottom and re-arm follow.
			scrollToBottom();
		}
	}

	/** Ctrl+↑/↓ jumps between user messages. A bare Ctrl chord (no meta/alt/shift)
	 *  so it doesn't clash with word-nav or browser shortcuts; a no-op with no
	 *  ticks, so it never swallows the keys on an empty conversation. */
	function onWindowKeydown(e: KeyboardEvent) {
		if (!e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return;
		if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
		if (ticks.length === 0) return;
		e.preventDefault();
		jumpUserMessage(e.key === 'ArrowDown' ? 1 : -1);
	}

	// Track collapsed state for reasoning items separately (index → collapsed)
	let collapsed = $state<Record<number, boolean>>({});
	// Whether the right detail rail (INFO + STATS) is shown. Persisted so the
	// choice survives reloads/navigation; defaults open. On narrow screens the
	// user can collapse it to give the conversation the full width.
	let detailOpen = $state(true);
	let copied = $state(false);
	/** Enter animation for stream items (DESIGN.md §3.2). Only LIVE items
	 *  animate: history arrives via the initial batch + upward mounting, both
	 *  of which render with the `history` transition (no motion) — see the
	 *  template's transition choice per item. */
	const itemEnter = rise(8, 200);
	/** No-op transition for history items (replay + upward-mounted batches):
	 *  they must appear in place, never fly in one batch after another. */
	const itemEnterHistory = rise(0, 0);

	function toggleDetail() {
		detailOpen = !detailOpen;
		localStorage.setItem('detailOpen', detailOpen ? '1' : '0');
		// Closing the rail also leaves inspect mode — its tab lives in the rail.
		if (!detailOpen) inspectMode = false;
	}

	onMount(() => {
		// Restore the persisted rail state; default open when unset.
		detailOpen = localStorage.getItem('detailOpen') !== '0';

		// Consume a pending fork (set by a fork click that navigated here) exactly
		// once. A fork always goes live-session -> draft route, which remounts this
		// component, so onMount is the right once-per-arrival hook. It must NOT live
		// in the sync $effect: that effect re-runs on tracked-dep changes and
		// takePendingFork() is one-shot, so a re-run would null the intent and send()
		// would open a plain new session instead of a fork. Only a draft can carry a
		// fork; a live-session mount just gets null and no-ops.
		if (routeSessionId === DRAFT_ID) {
			const pending = takePendingFork();
			forkIntent = pending;
			if (pending) {
				input = pending.draftText;
				// Show what this fork will inherit BEFORE the first send materializes the
				// session (until then there's no id, so no snapshot). Same dimmed render.
				void loadForkPreview(pending);
			}
		}

		// Re-measure minimap ticks whenever the scroll content resizes (new
		// messages, streaming growth, window resize, font load). One observer for
		// the lifetime of the component.
		const ro = new ResizeObserver(() => scheduleMeasure());
		if (streamEl) {
			ro.observe(streamEl);
			// The observer only fires on RESIZE — the very first content mount
			// precedes any resize, so kick one measurement explicitly.
			scheduleMeasure();
		}
		window.addEventListener('keydown', onWindowKeydown);
		return () => {
			ro.disconnect();
			cancelAnimationFrame(measureRaf);
			window.removeEventListener('keydown', onWindowKeydown);
		};
	});

	// The replay boundary folded in: reveal the (fully rendered) conversation
	// and land at the tail. Runs once per subscription.
	$effect(() => {
		if (convo.ready && replayedCount === 0 && !isDraft) presentReplayedHistory();
	});

	// Re-measure when the item list changes (covers content that grows without a
	// size change the observer would catch, e.g. a new user turn appended).
	$effect(() => {
		void convo.items.length;
		scheduleMeasure();
	});

	const isDraft = $derived(sessionId === DRAFT_ID);

	/** Returns `true` when the element is scrolled close enough to the bottom. */
	function isNearBottom(el: HTMLElement): boolean {
		return el.scrollHeight - el.scrollTop - el.clientHeight < SCROLL_BOTTOM_THRESHOLD;
	}

	/** Called whenever the user scrolls inside the stream container.
	 *  `shouldAutoScroll` models the user's INTENT to follow the live tail, so it
	 *  is only disarmed when the user scrolls UP (away from the bottom) — never by
	 *  the intermediate frames of a programmatic smooth scroll (which momentarily
	 *  read as "not at bottom" and would otherwise cancel following mid-animation,
	 *  the "clicked to bottom but new content stopped following" bug). Scrolling
	 *  down that reaches the bottom re-arms it. */
	let lastScrollTop = 0;
	function onStreamScroll() {
		if (!streamEl) return;
		const st = streamEl.scrollTop;
		if (st < lastScrollTop && !isNearBottom(streamEl)) {
			// Deliberate upward scroll away from the tail: stop following.
			shouldAutoScroll = false;
		} else if (isNearBottom(streamEl)) {
			// Back at (or scrolled down to) the bottom: follow the tail again.
			shouldAutoScroll = true;
		}
		lastScrollTop = st;
		// Track viewport-top position as a fraction of the FULL content height
		// (same basis as each tick's `top`), so the active-tick compare lines up.
		const total = streamEl.scrollHeight;
		scrollFrac = total > 0 ? st / total : 0;
	}

	/** Jump to the newest message and re-arm auto-scroll. Backs the "scroll to
	 *  bottom" affordance shown while the user is reading history. */
	function scrollToBottom() {
		if (!streamEl) return;
		shouldAutoScroll = true;
		streamEl.scrollTo({ top: streamEl.scrollHeight, behavior: 'smooth' });
	}

	/** Load session meta + config-layer runtime + summary snapshot for the right
	 *  detail panel. Best-effort: a failure here must not break the conversation
	 *  view, so each lookup owns its own try/clear and errors are swallowed. */
	async function loadMeta(id: string) {
		try {
			meta = await client.getSession(id);
		} catch {
			/* INFO panel just stays empty; conversation still works. */
		}
		// Resolve the config-layer model independently: a runtime failure must not
		// blank the meta we just loaded, and a stale value must not linger, so it
		// owns its own try/clear.
		try {
			runtime = await client.getRuntime(id);
		} catch {
			runtime = null;
		}
		// Sync the config picker to what this live session actually runs on, so it
		// shows the current profile/model and a change is detectable (cfgDirty).
		// The runtime model is a bare id; qualify it as `provider/model_id` to
		// match the picker option values.
		curProfile = meta?.profile_id ?? '';
		curModel = runtime ? `${runtime.provider}/${runtime.model}` : '';
		selProfile = curProfile;
		selModel = curModel;
		// Sync the per-turn effort picker to the session's effective tier.
		syncEffortFromRuntime();
		await refreshSummary(id);
		await loadInherited(id);
	}

	/** One rendered line of inherited context: the parent conversation a branched
	 *  session carries, flattened from the snapshot's `Message[]` into the few
	 *  kinds we display (system prompt is identity, not conversation, so skipped).
	 *  Tool results are folded onto their call. */
	type InheritedItem =
		| { kind: 'user'; text: string }
		| { kind: 'text'; text: string }
		| { kind: 'tool'; name: string; args: string; result?: string };

	/** Map a snapshot `Message[]` into `InheritedItem[]` for dimmed display:
	 *  System is dropped; Assistant text and each tool call become items; a Tool
	 *  result attaches to the most recent tool item (by call id) so it renders
	 *  under the call that produced it. */
	function mapInherited(messages: Message[]): InheritedItem[] {
		const items: InheritedItem[] = [];
		// call id → tool item, so a later Tool message finds the call it answers.
		const byCallId = new Map<string, InheritedItem & { kind: 'tool' }>();
		for (const msg of messages) {
			if ('System' in msg) continue;
			if ('User' in msg) {
				items.push({ kind: 'user', text: msg.User.content });
			} else if ('Assistant' in msg) {
				const text = msg.Assistant.content?.trim();
				if (text) items.push({ kind: 'text', text });
				for (const call of msg.Assistant.tool_calls ?? []) {
					const item = { kind: 'tool' as const, name: call.name, args: call.arguments };
					items.push(item);
					byCallId.set(call.id, item);
				}
			} else if ('Tool' in msg) {
				const call = byCallId.get(msg.Tool.tool_call_id);
				if (call) call.result = msg.Tool.content;
			}
		}
		return items;
	}

	/** Load a branched session's inherited context (dimmed history). Best-effort:
	 *  a `new` session has no snapshot (the endpoint 404s), and any failure just
	 *  leaves the header off — the live conversation still renders. Only fetched
	 *  when the origin says this session derives from a parent, so a plain new
	 *  session never makes the request. */
	async function loadInherited(id: string) {
		if (!meta || meta.origin.kind === 'new') {
			inherited = [];
			return;
		}
		try {
			inherited = mapInherited(await client.getSnapshot(id));
		} catch {
			inherited = []; // no snapshot / read failure → just omit the header
		}
	}

	/** Load a PENDING fork's inherited context for the draft view, before any
	 *  session exists. The gateway rebuilds the parent's context up to the branch
	 *  point (same computation the real fork uses to seed the child), so the draft
	 *  previews exactly what the branch will carry. Best-effort: any failure leaves
	 *  the header off — the draft still sends and forks normally. */
	async function loadForkPreview(p: PendingFork) {
		try {
			inherited = mapInherited(await client.getForkPreview(p.parentId, p.atSeq));
			// The preview renders ABOVE the input, so a fresh draft would otherwise sit
			// scrolled to the oldest inherited line. Wait for the DOM to grow with the
			// inherited block, then jump to the bottom — the branch point ("分支自此处")
			// and the pre-filled input, which is what the user acts on.
			await tick();
			// Land at the bottom instantly (no smooth animation): a fresh draft opening
			// should already BE at the branch point, not animate a long scroll down
			// through the history — same reasoning as the replay path's instant scroll.
			if (streamEl) {
				shouldAutoScroll = true;
				streamEl.scrollTo({ top: streamEl.scrollHeight, behavior: 'instant' });
			}
		} catch {
			inherited = []; // preview unavailable → just omit the header
		}
	}

	/** Pull the folded summary snapshot for the STATS panel. Best-effort: a fold
	 *  failure leaves the panel showing the last good value rather than blanking. */
	async function refreshSummary(id: string) {
		try {
			summary = await client.getSummary(id);
		} catch {
			/* keep prior summary */
		}
	}

	/** Debounced STATS refresh (Q2): coalesces the burst of per-request refreshes
	 *  in a long turn into one log-replay every ~500 ms, so metrics track the live
	 *  conversation per round without hammering the summary endpoint each request. */
	function scheduleSummaryRefresh(id: string) {
		clearTimeout(summaryDebounce);
		summaryDebounce = setTimeout(() => void refreshSummary(id), 500);
	}

	/** Open a session: fetch the server-folded view (one request, no replay
	 *  stream, no actor spawn), render it, then attach the live stream
	 *  resuming after the view's high-water seq. Falls back to the bare
	 *  subscription (full replay) if the view endpoint fails, so a session
	 *  always opens. */
	function subscribe(id: string) {
		sub?.close();
		if (flushRaf) {
			cancelAnimationFrame(flushRaf);
			flushRaf = 0;
		}
		eventBuffer = [];
		replayedCount = 0;
		convo = emptyState();
		collapsed = {};
		rawLog = [];
		inspectTick = 0;
		context = null;
		queued = loadQueue(id);
		shouldAutoScroll = true;
		loading = true;

		const myGen = ++subscribeGen;
		void (async () => {
			try {
				const view = await client.getView(id);
				if (myGen !== subscribeGen) return; // switched away mid-fetch
				convo = stateFromView(view);
				replayedCount = convo.items.length;
				loading = false;
				// Land at the tail once the view is painted.
				await tick();
				if (myGen !== subscribeGen || !streamEl) return;
				streamEl.scrollTop = streamEl.scrollHeight;
				lastScrollTop = streamEl.scrollTop;
				// Attach the live stream after the view's high-water seq: events
				// committed between the view read and this subscribe replay over
				// SSE (Last-Event-ID), so nothing is lost in the gap.
				attachLive(id, view.last_seq ?? undefined);
			} catch {
				if (myGen !== subscribeGen) return;
				// The view endpoint failed: fall back to the full replay stream so
				// the session still opens (the old path, slower but always works).
				loading = false;
				attachLive(id, undefined);
			}
		})();
	}

	/** Attach the live SSE stream, resuming after `lastSeq` (undefined = a
	 *  fresh subscribe: the gateway replays the full committed log first). */
	function attachLive(id: string, lastSeq: number | undefined) {
		sub = client.subscribeEvents(
			id,
			{
				onEvent: (ev) => {
					// Feed the status layer FIRST: a Turn::Started flips the sidebar
					// row to running even before the gateway's status hub publishes
					// (the actor dequeues the send later — e.g. while parked on an
					// approval from the previous turn).
					notifySessionEvent(ev);
					eventBuffer.push(ev);
					scheduleFlush();
					if (ev.type === 'event') {
						rawLog.push(ev);
						scheduleInspectTick();
					}
					if (ev.type === 'compacted') {
						// Compaction swaps the live session for a fresh one. Navigate to
						// its route; page.params.id updates → the sync $effect re-subscribes
						// and reloads meta for the new id (and the sidebar highlights it).
						void goto(`/workspaces/${workspaceId}/sessions/${ev.new_session_id}`);
					}
					// A settled turn means the fold's aggregates changed — refresh the
					// STATS snapshot so turns/cost/tokens track the live conversation.
					if (ev.type === 'turn_settled') {
						void refreshSummary(id);
						// LSP servers spawn/state-change during a turn's file ops, so the
						// INFO panel's LSP section is only fresh if runtime is refetched
						// once the turn settles (`doc/lsp.md` §5.1). Best-effort: keep the
						// last good value on failure rather than blanking the panel.
						client
							.getRuntime(id)
							.then((rt) => (runtime = rt))
							.catch(() => {});
						// The turn is done: release the next queued message (if any). One
						// per settle keeps the gateway's own defer-queue empty, so every
						// still-pending message stays here — visible and cancellable.
						void flushQueue(id);
					}
					// Live context occupancy (per round): drive the STATS context bar.
					if (ev.type === 'context_updated') {
						context = { tokens: ev.tokens, window: ev.window, threshold: ev.threshold };
					}
					// Per-request STATS refresh (Q2): a committed RequestCompleted means a
					// model round's usage landed, so the aggregates moved mid-turn.
					// Debounced; history already folded above is excluded by the seq dedup.
					if (ev.type === 'event') {
						const p = ev.payload;
						if ('Model' in p && 'RequestCompleted' in p.Model) {
							scheduleSummaryRefresh(id);
						}
					}
					// Follow-scroll lives in flushEvents: the DOM only changes after the
					// batched fold commits, so scrolling per raw event would chase frames
					// that do not exist yet.
				},
				onError: () => {
					// A dropped stream triggers the transport's reconnect loop — the
					// neutral reconnecting banner (onConnection('connecting') fires on
					// the retry) covers it; an error bar for a self-healing condition
					// would just alarm. Non-stream errors still surface via `error`.
				},
				onConnection: (state) => {
					connection = state;
					// Link restored: flush any message that failed to send while
					// offline (it sits at the queue head). No-op on the initial
					// connect — an empty queue returns immediately, and a running
					// turn defers the flush to the settle handler above. This also
					// covers page-load recovery: the queue was persisted to
					// localStorage, so a refresh during an outage resumes here.
					if (state === 'connected') void flushQueue(id);
				}
			},
			lastSeq
		);
	}

	$effect(() => {
		const id = sessionId;
		// Draft: show an empty conversation, don't subscribe or load meta. The
		// real session doesn't exist yet — it's created on the first send().
		if (id === DRAFT_ID) {
			sub?.close();
			replayedCount = 0;
			convo = emptyState();
			collapsed = {};
			meta = null;
			runtime = null;
			summary = null;
			// Keep a fork preview that onMount kicked off: this effect re-runs (see the
			// note below) and its async load may still be in flight, so an unconditional
			// clear would race-stomp it. A plain draft (no forkIntent) still clears.
			if (!forkIntent) inherited = [];
			context = null;
			// A draft has no persisted queue (nothing to key on, and its first send
			// creates the session); clear any carried over from the previous session.
			queued = [];
			// NOTE: the pending fork is consumed once in onMount, NOT here. This effect
			// re-runs whenever a tracked dep changes (e.g. loadConfigOptions below reads
			// profiles/models, then writes them after its await), and takePendingFork()
			// is one-shot — a second run would return null and clobber forkIntent, so
			// send() would create a plain new session instead of a fork.
			// Populate the config picker options (profiles + models). Best-effort:
			// a failure leaves the dropdowns empty and send still works on defaults.
			void loadConfigOptions();
			return;
		}
		subscribe(id);
		void loadMeta(id);
		// The picker is persistent (live sessions can reconfigure), so load its
		// profile/model options here too, not only on a draft.
		void loadConfigOptions();
	});

	// Mark this session seen up to the latest committed event, while it's the open
	// conversation. As its stream advances `convo.lastSeq`, ack it so the session
	// list flips this row unseen→seen (and a turn that finishes while it's focused
	// never shows as unseen). Drafts have no persisted session to ack.
	$effect(() => {
		const id = sessionId;
		const seq = convo.lastSeq;
		if (id !== DRAFT_ID && seq !== undefined) markSeen(id, seq);
	});

	// While any LSP server is `starting`, poll the runtime on a short cadence so
	// the "indexing…" hint clears as soon as the server runs/fails — the mount +
	// turn_settled refetches are too sparse for a transient that may resolve in
	// seconds (`doc/lsp.md` §5.2). The effect re-runs when the starting set
	// changes; the interval only lives while something is still starting.
	$effect(() => {
		const id = sessionId;
		if (id === DRAFT_ID || startingServers.length === 0) return;
		const timer = setInterval(() => {
			client
				.getRuntime(id)
				.then((rt) => (runtime = rt))
				.catch(() => {});
		}, 2000);
		return () => clearInterval(timer);
	});

	/** Load the profile + model lists for the config picker. Fetched once
	 *  (guarded), best-effort — a failure leaves the lists empty so the picker
	 *  just offers nothing (draft falls back to gateway defaults; a live session
	 *  simply can't reconfigure). */
	async function loadConfigOptions() {
		if (profiles.length > 0 || models.length > 0) return;
		try {
			[profiles, models] = await Promise.all([client.listProfiles(), client.listModels()]);
			// Resolve the default profile's configured model so a draft's model
			// picker can show what the session will run on (not a bare "default").
			const defName = profiles[0]?.name;
			if (defName) {
				const def = await client.getProfile(defName).catch(() => null);
				profileDefaultModel = def?.model?.default ?? null;
			}
		} catch {
			/* leave lists empty */
		}
	}

	/** Runtime-layer models that diverge from the configured model: models a
	 *  RequestStarted actually used (folded into convo.runtimeModels) that aren't
	 *  the config-layer selection (a subagent/fork on a different model). Empty
	 *  until the config model is known, so we never flag divergence we can't yet
	 *  judge. Fail-loud (CLAUDE.md #12); the displayed Model row stays the stable
	 *  config layer (B4). */
	const divergent = $derived(
		runtime ? [...convo.runtimeModels].filter((m) => m !== runtime!.model) : []
	);

	onDestroy(() => {
		sub?.close();
		if (flushRaf) cancelAnimationFrame(flushRaf);
		clearTimeout(summaryDebounce);
		clearTimeout(livenessTimer);
	});

	async function send() {
		const text = input.trim();
		// Archived sessions are read-only (the input is disabled; this is the
		// belt-and-suspenders guard for programmatic paths like Enter handlers).
		if (!text || sending || isArchived) return;
		// A turn is already running on this (real) session: queue the message
		// locally instead of POSTing. The gateway would accept it but defer it in
		// an invisible in-memory queue; holding it here keeps it visible as a
		// pending chip the user can cancel, and it flushes on the next settle.
		// Drafts never have a running turn, so they always fall through to send.
		if (!isDraft && turnRunning) {
			queued = enqueue(queued, text);
			persistQueue();
			input = '';
			return;
		}
		sending = true;
		error = null;
		try {
			if (sessionId === DRAFT_ID) {
				// Lazily create the real session on first send. A pending fork branches
				// from its parent at the recorded seq (inheriting that context); an
				// ordinary draft creates a fresh session. Either way the workspace is
				// fixed by `workspaceId` (path resolved server-side, never sent). The
				// picker's effective profile/model ride along verbatim — WYSIWYG: the
				// session is created with exactly what the picker shows, so the UI can
				// never display one model while the session silently uses another.
				const realId = forkIntent
					? await client.forkSession(forkIntent.parentId, forkIntent.atSeq)
					: await client.createWorkspaceSession(workspaceId, {
							profile: effProfile || undefined,
							model: effModel || undefined
						});
				// The fork is materialized; drop the intent so a later send can't reuse
				// it (defensive — we navigate away immediately after).
				forkIntent = null;
				// Send the first turn (committed to the log), then navigate to the real
				// session route. The draft and a real session are DIFFERENT routes
				// (`/workspaces/[wsId]` vs `.../sessions/[id]`), so a shallow
				// replaceState wouldn't rematch — the page would stay on the draft and
				// the sidebar wouldn't see the new id. A real goto mounts the session
				// page (which replays the just-committed turn over SSE) and updates
				// page.params.id so the sidebar highlights + inserts the new row.
				await client.sendMessage(realId, text, { thinkEffort: effortForSend() });
				input = '';
				// Same optimistic echo on the draft view: the goto below re-subscribes
				// and replays, but the bubble makes the accepted send visible in the
				// gap (and the draft view resets on navigation anyway).
				convo = pushOptimisticUser(convo, text);
				await goto(`/workspaces/${workspaceId}/sessions/${realId}`);
			} else if (profileDirty) {
				// Lazy profile switch (like fork): picking a new profile does NOT
				// reconfigure on its own — only sending materializes the switch, so a
				// misclick never spawns a reconfiguration session. The new session is
				// seeded with this conversation, then the message goes out on it.
				const newId = await client.reconfigure(sessionId, {
					profile: selProfile || undefined
				});
				await client.sendMessage(newId, text, {
					model: modelForSend(),
					thinkEffort: effortForSend()
				});
				input = '';
				await goto(`/workspaces/${workspaceId}/sessions/${newId}`);
			} else {
				const id = sessionId;
				const seqBefore = convo.lastSeq;
				// Model + effort are per-turn picks: they ride along on this message
				// only, never reconfiguring the session.
				await client.sendMessage(id, text, {
					model: modelForSend(),
					thinkEffort: effortForSend()
				});
				input = '';
				// Optimistic echo: the send was ACCEPTED (202) — show the bubble now
				// instead of waiting for Turn::Started to fold back. Normally a blink;
				// with a silently-dead stream it's the user's only proof the message
				// went out while the liveness check below re-attaches the stream.
				// The committed event replaces it (matched by text), so no duplicate.
				convo = pushOptimisticUser(convo, text);
				if (shouldAutoScroll) scrollTailSoon();
				// Post-send liveness check: the send succeeded (the gateway accepted
				// the turn), so a HEALTHY stream delivers its first event within a
				// few seconds. Silence past that means the stream died quietly
				// (half-open TCP) — the exact "backend is running but the UI never
				// updates until a refresh" case. Force a reconnect now instead of
				// waiting up to ~45s for the stall watchdog.
				clearTimeout(livenessTimer);
				livenessTimer = setTimeout(() => {
					if (sessionId !== id || eventBuffer.length > 0) return;
					if (convo.lastSeq !== seqBefore) return;
					sub?.reconnect?.();
				}, SEND_LIVENESS_MS);
			}
		} catch (e) {
			// A connectivity failure (offline, gateway down) enqueues the message
			// as a pending chip — the same affordance as mid-turn queueing: visible,
			// persisted, cancellable, and auto-sent on reconnect (see onConnection).
			// The user pressed send; "the network hiccuped" must not leave the text
			// sitting in the input waiting for a manual retry. A real REJECTION
			// (archived session, dead actor) stays an error bar — auto-retrying it
			// would loop forever, and the text stays put for the user to handle.
			if (!isDraft && isOfflineError(e)) {
				queued = enqueue(queued, text);
				persistQueue();
				input = '';
			} else {
				error = e instanceof Error ? e.message : String(e);
			}
		} finally {
			sending = false;
		}
	}

	/** True when `e` is a connectivity failure (fetch never got a response —
	 *  offline, DNS, gateway down) rather than a server REJECTION. `fetch` only
	 *  rejects with TypeError for network-level failure; any HTTP status (even
	 *  5xx) resolves and lands in gatewayError's `gateway <status>: ...` Error
	 *  instead. The message POST is fire-and-forget (202 = enqueued), so a
	 *  network failure means it could not have been accepted — queueing it for
	 *  retry can never duplicate a turn. */
	function isOfflineError(e: unknown): boolean {
		return e instanceof TypeError;
	}

	/** Persist the current pending queue for this session. A no-op key-clear when
	 *  empty (see queue.ts), so a drained session leaves no dead entry. Skipped
	 *  for drafts, which have no real id to key on yet. */
	function persistQueue() {
		if (sessionId !== DRAFT_ID) saveQueue(sessionId, queued);
	}

	/** Release the oldest pending message now that the turn settled: POST it and
	 *  drop it from the queue. Only one per settle — the next flushes when this
	 *  message's own turn settles, so the gateway's invisible defer-queue stays
	 *  empty and every still-pending message remains a cancellable chip. Guarded
	 *  on `id` matching the live session so a settle from a session we just left
	 *  can't flush into the wrong conversation. */
	async function flushQueue(id: string) {
		if (id !== sessionId || sending) return;
		const next = queued[0];
		if (!next) return;
		queued = removeFromQueue(queued, next.id);
		persistQueue();
		sending = true;
		error = null;
		try {
			await client.sendMessage(id, next.text);
		} catch (e) {
			// Re-queue at the FRONT on failure so nothing is silently dropped
			// (fail loud). A connectivity failure stays QUIET — the reconnecting
			// banner already says the link is down, and the next 'connected' (or
			// turn settle) retries. A real rejection surfaces as an error bar.
			queued = [next, ...queued];
			persistQueue();
			if (!isOfflineError(e)) error = e instanceof Error ? e.message : String(e);
		} finally {
			sending = false;
		}
	}

	/** Cancel a pending message: remove it and drop its text back into the input
	 *  so the user can edit and resend (cancelling is often "let me add more"
	 *  rather than "discard"). Appends to any half-typed input rather than
	 *  clobbering it. */
	function cancelQueued(item: QueuedMessage) {
		queued = removeFromQueue(queued, item.id);
		persistQueue();
		input = input.trim() ? `${input.trimEnd()}\n${item.text}` : item.text;
	}

	async function cancel() {
		try {
			await client.cancel(sessionId);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	// Answer a permission `ask` (doc/permission.md §5). The decision + scope is
	// delivered to the suspended turn over the actor; the card's pending flag
	// clears when the committed `Permission::Decided` event folds back in.
	async function decideApproval(
		callId: string,
		decision: 'approve' | 'reject',
		scope: ApprovalScope
	) {
		try {
			await client.approve(sessionId, callId, decision, scope);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			// Rethrow so ApprovalControls' own catch re-enables its buttons: a failed
			// approve (dropped connection, dead session) must not freeze the card on
			// a permanent "处理中…". The card stays pending (no Decided folds), so
			// the user can retry once the buttons are live again.
			throw e;
		}
	}

	function onKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter' && !e.shiftKey) {
			e.preventDefault();
			void send();
		}
	}

	function toggleCollapse(item: Item, i: number) {
		// Flip from the currently *displayed* state, not the raw map value.
		// Auto-collapsed items have no map entry yet, so `!collapsed[i]` would
		// compute `true` (= still collapsed) on first click, needing a second
		// click to take effect. Seed from the displayed default so one click
		// always flips.
		collapsed = { ...collapsed, [i]: !isCollapsedDefault(item, i) };
	}

	/** The collapse state an item shows when the override map has no entry:
	 *  reasoning always starts collapsed (streaming shows a ticker line, done a
	 *  preview); a finished todo folds; everything else opens. Mirrors the
	 *  defaults ConversationItem applies when rendering. */
	function isCollapsedDefault(item: Item, i: number): boolean {
		if (i in collapsed) return collapsed[i];
		if (item.kind === 'reasoning') return true;
		if (item.kind === 'todo') {
			return (
				item.steps.length > 0 &&
				item.steps.every(
					(s) => s.status === 'completed' || s.status === 'cancelled' || s.status === 'blocked'
				)
			);
		}
		return false;
	}

	/** Short session label for the topbar: prefer the latest user message would
	 *  be ideal, but we don't track titles yet — show a workspace-derived label
	 *  or the session id. */
	function topbarTitle(): string {
		if (sessionId === DRAFT_ID) return 'New session';
		if (meta?.workspace) {
			const parts = meta.workspace.split('/').filter(Boolean);
			return parts[parts.length - 1] ?? sessionId;
		}
		return shortId(sessionId);
	}

	function shortId(id: string): string {
		return id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
	}

	async function copyId() {
		try {
			navigator.clipboard.writeText(sessionId);
			copied = true;
			setTimeout(() => (copied = false), 2000);
		} catch {
			/* clipboard unavailable */
		}
	}

	const incomplete = $derived(convo.lastSettle != null);
	// Cancel only makes sense while a turn is running (the backend ignores Cancel
	// when idle), so the button is shown only then — see ConversationState.turnRunning.
	const turnRunning = $derived(convo.turnRunning === true);

	// Servers still in their `starting` transient (spawned + handshook, not yet
	// answering — `doc/lsp.md` §5.2). The composer shows a transient "indexing…"
	// hint for these so a slow index never reads as the app hanging. Empty when
	// every server is running/failed — the hint disappears on its own.
	const startingServers = $derived(
		(runtime?.lsp ?? []).filter((s) => s.state === 'starting')
	);

	// The seq of the FIRST user message in the view. Forking before it would
	// inherit an empty context (identical to a plain new session), so that first
	// message alone gets no fork affordance — every later user turn does.
	const firstUserSeq = $derived.by<number | null>(() => {
		for (const it of convo.items) {
			if (it.kind === 'user' && it.seq != null) return it.seq;
		}
		return null;
	});

	// The todo list shown in the sticky dock: the latest committed todo card (running
	// OR finished). Once any todo list exists it stays docked, so a later list swaps in
	// place rather than the dock vanishing and a new one popping in abruptly.
	// Older todo lists (superseded by a newer `init`) fall back to inline history.
	// Carrying the index lets the inline render skip it — shown in one place only.
	const dockTodo = $derived.by<{ steps: TodoStep[]; index: number } | null>(() => {
		for (let i = convo.items.length - 1; i >= 0; i--) {
			const it = convo.items[i];
			// Skip streaming placeholders (empty, half-arrived) so the dock never
			// blanks between ops; the last committed card is the live list.
			if (it.kind === 'todo' && !it.streaming && it.steps.length > 0) {
				return { steps: it.steps, index: i };
			}
		}
		return null;
	});

	// Collapsed state for the sticky dock. Defaults collapsed so the todo list + input
	// don't eat a big vertical slab; the user expands to see steps. Separate from
	// the inline-item `collapsed` map (keyed by item index).
	let pinnedTodoCollapsed = $state(true);

	/** The default profile's configured model (`provider/model_id`), loaded
	 *  once for a draft so the model picker can show what the session WILL run
	 *  on when nothing is picked. Best-effort: a failure leaves it null and the
	 *  trigger just shows the generic fallback. */
	let profileDefaultModel = $state<string | null>(null);

	/** The effective profile: the pick, or the gateway default profile (first
	 *  in the list) when unpicked — a draft always shows what it WILL use. */
	const effProfile = $derived(selProfile || (profiles[0]?.name ?? ''));
	/** The effective model (`provider/model_id`): the pick, else the profile
	 *  default's qualified form when it appears in the usable list, else the
	 *  first usable model (a configured default that no longer resolves — e.g.
	 *  its provider lost credentials — must not leave the picker blank). */
	const effModel = $derived.by(() => {
		if (selModel) return selModel;
		const def = profileDefaultModel;
		if (def && models.some((m) => `${m.provider}/${m.model_id}` === def)) return def;
		const first = models[0];
		return first ? `${first.provider}/${first.model_id}` : '';
	});

	// A live session syncs from its runtime (loadMeta): the configured default
	// effort when the model declares it, else the model's official default (the
	// last declared tier), so the picker always shows a concrete tier.
	function syncEffortFromRuntime() {
		const tiers = runtime?.think_efforts ?? [];
		if (tiers.length === 0) {
			selEffort = '';
			return;
		}
		const cfg = runtime?.think_effort;
		selEffort = cfg && tiers.includes(cfg) ? cfg : (tiers.at(-1) ?? '');
	}

	// A draft has no runtime; once its effective model resolves, adopt that
	// model's default tier (its last declared one) so the effort picker shows a
	// concrete tier from the start. Guarded to run once per model resolution —
	// a manual pick or a model change owns the value from there.
	$effect(() => {
		if (!isDraft) return;
		const entry = models.find((m) => `${m.provider}/${m.model_id}` === effModel);
		if (entry && !selEffort) selEffort = entry.think_efforts.at(-1) ?? '';
	});

	/** Short labels for the three pickers (profile / model / effort): always
	 *  the concrete value in effect — on a draft what the session will be
	 *  created on, on a live session what it runs on (a differing profile pick
	 *  = pending lazy switch; model/effort = per-turn picks for the next send). */
	const profileLabel = $derived(effProfile || 'default');
	const modelLabel = $derived.by(() => {
		if (effModel) return effModel.split('/').pop() ?? effModel;
		return 'no model configured';
	});

	// Picker option lists (shared `ModelSelect` rows). The model list is the
	// usable models (credentials-resolved server-side). No synthetic "Default"
	// row — the pickers always show a concrete current value; on a draft the
	// gateway/profile default applies when nothing was picked (the trigger
	// still shows what WILL be used, resolved below).
	const profileOptions = $derived<SelectOption[]>(
		profiles.map((p) => ({
			value: p.name,
			label: p.name,
			detail: p.description ?? undefined
		}))
	);
	const modelOptions = $derived<SelectOption[]>(
		models.map((m) => ({
			value: `${m.provider}/${m.model_id}`,
			label: m.model_id,
			detail: m.provider
		}))
	);
	// The effort tiers of the model the picker currently points at. On a live
	// session the runtime info is authoritative; a draft (or a pending model
	// change) falls back to the catalog entry for the selected model.
	const curModelEfforts = $derived.by<string[]>(() => {
		const fromRuntime = !isDraft && effModel === curModel ? (runtime?.think_efforts ?? []) : [];
		if (fromRuntime.length > 0) return fromRuntime;
		return models.find((m) => `${m.provider}/${m.model_id}` === effModel)?.think_efforts ?? [];
	});
	const effortOptions = $derived<SelectOption[]>(
		curModelEfforts.map((t) => ({ value: t, label: t }))
	);
	// The model's official default tier: the profile's configured tier when the
	// model declares it, else the LAST declared tier (provider catalogs list
	// tiers ascending, so the last is the strongest/default — Kimi K3 defaults
	// to `max`). Effort has no separate "off" state here: a model with tiers
	// always reasons at one of them, so the picker shows that tier.
	const effortDefault = $derived.by(() => {
		const rt = !isDraft && effModel === curModel ? runtime?.think_effort : null;
		if (rt && curModelEfforts.includes(rt)) return rt;
		return curModelEfforts.at(-1) ?? '';
	});
	const effortLabel = $derived(
		selEffort && curModelEfforts.includes(selEffort) ? selEffort : effortDefault
	);

	/** A model switch adopts the new model's default tier (its last declared
	 *  one) so the picker never shows a tier the model doesn't declare. */
	function onModelPick(v: string) {
		selModel = v;
		const entry = models.find((m) => `${m.provider}/${m.model_id}` === v);
		selEffort = entry?.think_efforts.at(-1) ?? '';
	}

	/** The effort tier to attach to the next send: the picker's pick when it is
	 *  a tier the current model declares, else undefined (the session's
	 *  configured default applies). A stale pick from a previous model is never
	 *  sent to a model that would reject it. */
	function effortForSend(): string | undefined {
		return selEffort && curModelEfforts.includes(selEffort) ? selEffort : undefined;
	}

	/** The per-turn model override to attach to the next send on a live session:
	 *  the pick only when it differs from what the session runs on (empty /
	 *  unchanged = no override, the session's configured model applies). */
	function modelForSend(): string | undefined {
		return !isDraft && selModel && selModel !== curModel ? selModel : undefined;
	}

	// Profile switching (live sessions only): picking a different profile does
	// NOT reconfigure on its own — the change is materialized lazily by the next
	// send (reconfigure → send → navigate), like fork's lazy semantics, so a
	// misclick never spawns a reconfiguration session. Until then the input
	// shows a quiet banner that sending will switch.
	let curProfile = $state('');
	let curModel = $state('');

	/** A live session has a pending lazy profile switch when the picker's
	 *  profile differs from what the session currently runs on. */
	const profileDirty = $derived(!isDraft && selProfile !== curProfile);

	// Fork: branch a new session from a chosen user turn. Like "new session", the
	// branch is created LAZILY — clicking fork navigates to the workspace draft
	// carrying a pending-fork intent (parent + branch point + the message text to
	// edit), and only the draft's first send materializes it (forkSession +
	// sendMessage). So a stray click that's never sent leaves no empty session.
	// The branch inherits the parent's context up to (but NOT including) the
	// chosen message — that message's text is pre-filled into the input to edit
	// and resend as the branch's first turn. `forkIntent`, when set on a draft,
	// is what makes send() fork instead of creating a plain session.
	let forkIntent = $state<PendingFork | null>(null);

	/** Start a fork from the user message at `msgSeq` (its committed Turn::Started
	 *  seq). Records the intent (parent = this session; branch point = the event
	 *  just before the message, so the chosen turn itself is excluded; text = the
	 *  message, to edit + resend) and navigates to the workspace draft. No-op on a
	 *  draft (nothing committed to branch from). The draft's send() does the work.
	 *  The first user turn (`msgSeq` at/near 0) is not offered a fork button, so we
	 *  never compute a negative branch point here. */
	function fork(msgSeq: number, text: string) {
		if (isDraft) return;
		setPendingFork({ parentId: sessionId, atSeq: msgSeq - 1, draftText: text });
		void goto(`/workspaces/${workspaceId}`);
	}
</script>

<div class="session-grid" class:no-detail={isDraft || !detailOpen}>
	<div class="conv-page">
		<!-- TOPBAR -->
		<div class="topbar">
			<a href="/" class="topbar-back" title="Back to home" aria-label="Back to home">
				<svg
					width="14"
					height="14"
					viewBox="0 0 14 14"
					fill="none"
					stroke="currentColor"
					stroke-width="1.6"
					stroke-linecap="round"
					stroke-linejoin="round"
				>
					<polyline points="8.5,2.5 4,7 8.5,11.5" />
				</svg>
			</a>
			<span class="topbar-title">{topbarTitle()}</span>
			<div class="topbar-sep"></div>
			<div class="topbar-meta">
				{#if isDraft}
					<span class="mono draft-hint">draft · created on first send</span>
				{:else}
					<button class="session-id-btn" onclick={copyId} title={copied ? 'Copied!' : sessionId}>
						{shortId(sessionId)}
						{#if copied}<span class="copy-toast" transition:fade={fadeIn()}>Copied!</span>{/if}
					</button>
					{#if incomplete}
						<span class="topbar-badge badge-running">incomplete</span>
					{/if}
				{/if}
			</div>
			{#if !isDraft}
				<button
					class="detail-toggle"
					class:on={detailOpen}
					onclick={toggleDetail}
					title={detailOpen ? 'Collapse detail panel' : 'Expand detail panel'}
					aria-label="Toggle detail panel"
					aria-pressed={detailOpen}
				>
					<svg
						width="14"
						height="14"
						viewBox="0 0 14 14"
						fill="none"
						stroke="currentColor"
						stroke-width="1.4"
						stroke-linecap="round"
						stroke-linejoin="round"
					>
						<rect x="1.5" y="2.5" width="11" height="9" rx="1.5" />
						<line x1="9" y1="2.5" x2="9" y2="11.5" />
					</svg>
				</button>
			{/if}
		</div>

		{#if error}
			<div class="error-bar">{error}</div>
		{/if}
		{#if connection === 'connecting' && convo.ready}
			<!-- Reconnect banner: only AFTER the first attach (convo.ready) — the
			     initial connect is covered by the loading skeleton, and showing
			     "reconnecting" before anything ever connected would be wrong. -->
			<div class="reconnect-bar" role="status">
				<span class="reconnect-dot" aria-hidden="true"></span>Connection lost, reconnecting…
			</div>
		{/if}

		<!-- CONVERSATION SCROLL -->
		<div class="conv-viewport">
			<div class="conv-scroll" bind:this={streamEl} onscroll={onStreamScroll}>
				<!-- Loading skeleton, OUTSIDE the pre-ready container (which is
				     visibility:hidden while loading, so anything inside it is
				     invisible too). Shows while the view loads; the conversation
				     below it appears already positioned at the tail. -->
				{#if (loading || !convo.ready) && !isDraft}
					<div class="loading-skeleton" role="status" aria-label="Loading history">
						<div class="sk-row sk-user">
							<Skeleton width="42%" height="38px" radius="var(--radius-lg)" />
						</div>
						<div class="sk-row"><Skeleton width="88%" height="14px" /></div>
						<div class="sk-row"><Skeleton width="76%" height="14px" /></div>
						<div class="sk-row">
							<Skeleton width="94%" height="52px" radius="var(--radius-md)" />
						</div>
						<div class="sk-row"><Skeleton width="64%" height="14px" /></div>
						<div class="sk-row sk-user">
							<Skeleton width="36%" height="38px" radius="var(--radius-lg)" />
						</div>
						<div class="sk-row"><Skeleton width="82%" height="14px" /></div>
						<div class="sk-row"><Skeleton width="70%" height="14px" /></div>
					</div>
				{/if}
				<div class="conv-inner" class:pre-ready={(loading || !convo.ready) && !isDraft}>
					<!-- Inherited context: the parent conversation this branched session was
				     seeded with (fork/compaction/reconfiguration), rendered dimmed above the
				     live turns so the branch shows what came before. See DESIGN.md §4.8. -->
					{#if inherited.length > 0}
						<div class="inherited" aria-label="Inherited context">
							{#each inherited as it, k (k)}
								{#if it.kind === 'user'}
									<div class="item item-user">
										<div class="user-bubble item-text">
											{#if browser}
												<!-- eslint-disable-next-line svelte/no-at-html-tags -->
												{@html renderUserMarkdown(it.text)}
											{:else}
												{it.text}
											{/if}
										</div>
									</div>
								{:else if it.kind === 'text'}
									<div class="item item-text inherited-text">
										{#if browser}
											<!-- eslint-disable-next-line svelte/no-at-html-tags -->
											{@html renderMarkdown(it.text)}
										{:else}
											{it.text}
										{/if}
									</div>
								{:else if it.kind === 'tool'}
									<div class="item inherited-tool">
										<span class="inherited-tool-name">{it.name}</span>
										{#if it.args && it.args !== '{}'}<span class="inherited-tool-args"
												>{it.args}</span
											>{/if}
									</div>
								{/if}
							{/each}
							<div class="inherited-sep">
								<span>Branched from here · inherited context above</span>
							</div>
						</div>
					{/if}
					{#each convo.items as item, i (item.id)}
						<ConversationItem
							{item}
							index={i}
							enterAnim={i < replayedCount ? itemEnterHistory : itemEnter}
							{isDraft}
							{firstUserSeq}
							dockTodoIndex={dockTodo?.index}
							{collapsed}
							onFork={fork}
							onToggleCollapse={toggleCollapse}
							onDecide={decideApproval}
						/>
					{/each}

					{#if convo.items.length === 0 && !loading && convo.ready}
						<p class="empty">
							{isDraft
								? 'Type a message to start a new conversation'
								: 'Send a message to start the conversation'}
						</p>
					{/if}
				</div>
			</div>

			{#if ticks.length > 1}
				<!-- User-message minimap: a tick per user turn on the scroll rail.
				     Hover previews the message; click (or Ctrl+↑/↓) jumps to it.
				     Shown only with >1 user message — a single tick is noise. -->
				<div class="minimap" aria-hidden="true">
					{#each ticks as tick (tick.index)}
						<button
							class="minimap-tick"
							class:active={tick.index === activeTickIndex}
							style="top: {tick.top * 100}%"
							onclick={() => scrollToUserMessage(tick.index)}
							tabindex="-1"
						>
							<span class="minimap-preview">{tick.preview}</span>
						</button>
					{/each}
				</div>
			{/if}

			{#if !shouldAutoScroll && convo.items.length > 0}
				<!-- Scroll-to-bottom: shown only while the user has scrolled up to read
				     history. Neutral surface (canvas-float), deliberately NOT the lime
				     accent — that's reserved for the user bubble + the one Send CTA. -->
				<button
					class="to-bottom"
					onclick={scrollToBottom}
					title="Back to bottom"
					aria-label="Scroll to latest"
					transition:fly={rise(8, 120)}
				>
					<svg
						width="16"
						height="16"
						viewBox="0 0 16 16"
						fill="none"
						stroke="currentColor"
						stroke-width="1.6"
						stroke-linecap="round"
						stroke-linejoin="round"
						aria-hidden="true"
					>
						<path d="M8 3v8" />
						<path d="M4.5 7.5 8 11l3.5-3.5" />
					</svg>
				</button>
			{/if}
		</div>

		<!-- ACTIVE TODO (sticky above input) — the latest todo list stays docked (running
	     or done) so a later list swaps in place instead of popping in abruptly.
	     Default collapsed to spare vertical space; the dock carries the divider
	     line so it reads as one zone with the input below. -->
		{#if dockTodo}
			<div class="plan-dock">
				<div class="plan-dock-inner">
					<TodoCard
						steps={dockTodo.steps}
						pinned
						expanded={!pinnedTodoCollapsed}
						onToggle={() => (pinnedTodoCollapsed = !pinnedTodoCollapsed)}
					/>
				</div>
			</div>
		{/if}

		<!-- INPUT AREA -->
		<div class="input-area" class:seamless={dockTodo}>
			<div class="input-inner">
				{#if queued.length > 0}
					<!-- Pending queue: messages the user sent while a turn was running.
					     They flush one-at-a-time on each settle; until then each is a
					     cancellable chip (× drops it back into the input to edit/resend). -->
					<div class="queue" aria-label="Queued messages">
						{#each queued as item (item.id)}
							<div class="queue-chip" transition:scale={pop(120)}>
								<span class="queue-dot" aria-hidden="true"></span>
								<span class="queue-text" title={item.text}>{item.text}</span>
								<button
									class="queue-cancel"
									onclick={() => cancelQueued(item)}
									title="Cancel and move back to the input"
									aria-label="Cancel queued message"
								>
									<svg
										width="10"
										height="10"
										viewBox="0 0 10 10"
										fill="none"
										stroke="currentColor"
										stroke-width="1.5"
										stroke-linecap="round"
									>
										<line x1="1" y1="1" x2="9" y2="9" />
										<line x1="9" y1="1" x2="1" y2="9" />
									</svg>
								</button>
							</div>
						{/each}
					</div>
				{/if}
				<div class="input-box">
					<textarea
						class="input-field"
						bind:value={input}
						onkeydown={onKeydown}
						disabled={isArchived}
						placeholder={isArchived
							? 'Session archived, read-only'
							: 'Type a message… Enter to send, Shift+Enter for newline'}
						rows="2"
					></textarea>
					<div class="input-actions">
						<span class="input-status">
							{#if turnRunning}
								<!-- Live turn indicator: the only place the running state is
								     surfaced in the conversation itself (the session-list
								     icon is per-row; this is the in-context signal). -->
								<span class="status-running">
									<span class="status-running-dot"></span>Running
								</span>
							{:else if startingServers.length > 0}
								<!-- LSP indexing transient (`doc/lsp.md` §5.2): a server in
								     its `starting` state is indexing, so a slow first answer
								     never reads as the app hanging. Amber, chinese font; it
								     disappears on its own once the server runs/fails. -->
								<span class="status-warn lsp-starting">
									{startingServers[0].name} 索引中…{#if startingServers.length > 1} +{startingServers.length - 1}{/if}
								</span>
							{:else if incomplete}
								<span class="status-warn">Turn incomplete</span>
							{/if}
						</span>
						<div class="pickers">
							<!-- Profile: a config-level pick. On a live session a change
							     is LAZY — it reconfigures (new session seeded with this
							     conversation) only when the next message sends. -->
							<PickerSelect
								options={profileOptions}
								bind:value={selProfile}
								key="profile"
								label={profileLabel}
								title="Profile (agent identity: prompt + tools){isDraft ? '' : ' — switching reconfigures on the next send'}"
							/>

							<!-- Model + effort: per-turn picks, riding along on the next
							     send — never a reconfiguration. -->
							<PickerSelect
								options={modelOptions}
								value={selModel}
								onselect={onModelPick}
								key="model"
								label={modelLabel}
								title="Model for the next turn (per-turn, no reconfiguration)"
							/>

							{#if curModelEfforts.length > 0}
								<PickerSelect
									options={effortOptions}
									bind:value={selEffort}
									key="effort"
									label={effortLabel}
									title="Reasoning effort for the next turn"
								/>
							{/if}
						</div>
						{#if !isDraft && turnRunning}
							<button class="input-btn cancel" onclick={cancel}>
								<svg
									width="10"
									height="10"
									viewBox="0 0 10 10"
									fill="none"
									stroke="currentColor"
									stroke-width="1.5"
									stroke-linecap="round"
								>
									<line x1="1" y1="1" x2="9" y2="9" />
									<line x1="9" y1="1" x2="1" y2="9" />
								</svg>
								Cancel
							</button>
						{/if}
						<button
							class="send-btn"
							disabled={isArchived || sending || !input.trim()}
							onclick={send}
							title={isArchived
								? 'Session archived, read-only'
								: !isDraft && turnRunning
									? 'Queue message (sent when the current turn ends) · Enter'
									: 'Send · Enter'}
							aria-label="Send message"
						>
							{#if sending}
								<span class="send-spinner" aria-hidden="true"></span>
							{:else}
								<!-- Original monoline "send" glyph: an upward stroke rising from a
								     base line — a launch/submit metaphor drawn from scratch (not a
								     borrowed paper-plane icon set). -->
								<svg
									class="send-icon"
									width="16"
									height="16"
									viewBox="0 0 16 16"
									fill="none"
									stroke="currentColor"
									stroke-width="1.6"
									stroke-linecap="round"
									stroke-linejoin="round"
									aria-hidden="true"
								>
									<path d="M8 12.5V4" />
									<path d="M4.5 7.5 8 4l3.5 3.5" />
									<path d="M4 13.5h8" />
								</svg>
							{/if}
						</button>
					</div>
				</div>
				<div class="input-hint">
					{#if profileDirty}
						<!-- Lazy profile switch: the pick alone changes nothing; sending
						     materializes it (reconfigure → new session seeded with this
						     conversation). Quiet warning, not an accent action. -->
						<span class="profile-note"
							>Profile → <strong>{selProfile || 'default'}</strong> · sending switches (new session
							seeded with this conversation)</span
						>
					{:else}
						Type / for commands
					{/if}
				</div>
			</div>
		</div>
	</div>

	{#if !isDraft}
		<DetailRail
			{inspectMode}
			bind:inspectReversed
			{inspectTick}
			{inspectRows}
			{meta}
			{runtime}
			{divergent}
			liveContext={context}
			{summary}
			onSetInspect={setInspect}
			onScrollToSeq={scrollToSeq}
		/>
	{/if}
</div>

<style>
	/* 2-col shell: conversation flexes, the detail rail is a fixed reading width.
	 * Fills the main area's full height; each column owns its own scroll. The
	 * draft state has no detail (no meta/summary yet), so it collapses to 1 col. */
	.session-grid {
		display: grid;
		grid-template-columns: 1fr 300px;
		height: 100%;
		overflow: hidden;
		min-width: 0;
	}

	.session-grid.no-detail {
		grid-template-columns: 1fr;
	}

	/* Collapsed: the rail is still in the DOM (toggle lives in the topbar), but
	 * the grid drops its column — hide it so it doesn't overflow the single col. */
	.session-grid.no-detail :global(.detail) {
		display: none;
	}

	.conv-page {
		display: flex;
		flex-direction: column;
		height: 100%;
		overflow: hidden;
		min-width: 0;
	}

	/* ---- TOPBAR ---- */
	.topbar {
		height: 44px;
		min-height: 44px;
		border-bottom: 1px solid var(--border-subtle);
		display: flex;
		align-items: center;
		padding: 0 var(--space-6);
		gap: var(--space-3);
		background: var(--canvas-raised);
		flex-shrink: 0;
	}

	.topbar-back {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		border-radius: var(--radius-sm);
		color: var(--text-tertiary);
		flex-shrink: 0;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.topbar-back:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	/* Detail-rail toggle: pinned to the topbar's right edge. `on` = rail open. */
	.detail-toggle {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		margin-left: auto;
		border-radius: var(--radius-sm);
		border: 1px solid transparent;
		background: transparent;
		color: var(--text-tertiary);
		cursor: pointer;
		flex-shrink: 0;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
	}

	.detail-toggle:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	.detail-toggle.on {
		color: var(--text-secondary);
		border-color: var(--border-default);
		background: var(--canvas-overlay);
	}

	.topbar-title {
		font-size: 13px;
		font-weight: 500;
		color: var(--text-primary);
		letter-spacing: -0.01em;
		font-family: var(--font-chinese);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 320px;
	}

	.topbar-sep {
		width: 1px;
		height: 14px;
		background: var(--border-default);
		flex-shrink: 0;
	}

	.topbar-meta {
		font-size: 11.5px;
		color: var(--text-tertiary);
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.mono {
		font-family: var(--font-mono);
		font-variant-numeric: tabular-nums;
	}

	/* ---- SESSION ID COPY BUTTON ---- */
	.session-id-btn {
		all: unset;
		cursor: pointer;
		font-family: var(--font-mono);
		font-variant-numeric: tabular-nums;
		font-size: 11.5px;
		color: var(--text-tertiary);
		padding: 2px 6px;
		border-radius: var(--radius-sm);
		position: relative;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}
	.session-id-btn:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}
	.copy-toast {
		position: absolute;
		left: 50%;
		top: calc(100% + 4px);
		transform: translateX(-50%);
		font-size: 10px;
		white-space: nowrap;
		background: var(--canvas-overlay);
		border: 1px solid var(--border-default);
		color: var(--text-secondary);
		padding: 2px 6px;
		border-radius: var(--radius-sm);
		box-shadow: var(--shadow-md);
		font-family: var(--font-mono);
		pointer-events: none;
		z-index: 10;
	}
	.draft-hint {
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.topbar-badge {
		padding: 2px 6px;
		border-radius: 3px;
		font-size: 10.5px;
		font-weight: 510;
		letter-spacing: 0.03em;
	}

	.badge-running {
		background: var(--state-running-bg);
		color: var(--state-running-text);
		border: 1px solid color-mix(in srgb, var(--state-running) 25%, transparent);
	}

	.error-bar {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		padding: var(--space-2) var(--space-6);
		border-bottom: 1px solid color-mix(in srgb, var(--state-error) 25%, transparent);
		font-size: 12.5px;
		flex-shrink: 0;
	}

	/* Reconnect banner: the running-state amber, not error red — a transient,
	 *  self-healing condition, not a failure. Same layout as .error-bar. */
	.reconnect-bar {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		color: var(--state-running-text);
		background: var(--state-running-bg);
		padding: var(--space-2) var(--space-6);
		border-bottom: 1px solid color-mix(in srgb, var(--state-running) 25%, transparent);
		font-size: 12.5px;
		flex-shrink: 0;
		font-family: var(--font-chinese);
	}
	.reconnect-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--state-running);
		animation: pulse 1.4s ease-in-out infinite;
		flex-shrink: 0;
	}
	@media (prefers-reduced-motion: reduce) {
		.reconnect-dot {
			animation: none;
		}
	}

	/* ---- CONVERSATION ---- */
	/* Positioning context for the minimap overlay; owns the flex height so the
	   inner scroll area and the pinned minimap share the same box. */
	.conv-viewport {
		flex: 1;
		position: relative;
		min-height: 0;
		display: flex;
	}

	.conv-scroll {
		flex: 1;
		overflow-y: auto;
		padding: var(--space-5) var(--space-10) var(--space-6);
		min-height: 0;
	}

	.conv-inner {
		max-width: 740px;
		margin: 0 auto;
	}

	/* Pre-ready (replay still folding): keep the conversation rendered but
	 *  invisible — it must exist in the layout so the reveal lands at the
	 *  correct tail position in a single frame. */
	.conv-inner.pre-ready {
		visibility: hidden;
	}

	/* ---- USER-MESSAGE MINIMAP (jump-to-message rail) ---- */
	.minimap {
		position: absolute;
		top: var(--space-2);
		bottom: var(--space-2);
		right: 3px;
		width: 10px;
		z-index: var(--z-sticky);
		pointer-events: none;
	}

	.minimap-tick {
		position: absolute;
		right: 0;
		/* Center the tick on its target fraction. */
		transform: translateY(-50%);
		display: flex;
		align-items: center;
		justify-content: flex-end;
		height: 12px;
		padding: 0;
		border: none;
		background: transparent;
		cursor: pointer;
		pointer-events: auto;
	}

	/* The visible dash. Widens + takes the accent on hover / when active. */
	.minimap-tick::after {
		content: '';
		width: 8px;
		height: 2px;
		border-radius: 1px;
		background: var(--text-disabled);
		transition:
			width var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.minimap-tick:hover::after,
	.minimap-tick.active::after {
		width: 10px;
		background: var(--accent);
	}

	/* Hover preview: the message's first line, floated to the left of the rail. */
	.minimap-preview {
		position: absolute;
		right: 16px;
		top: 50%;
		transform: translateY(-50%);
		max-width: 260px;
		padding: 4px 8px;
		border-radius: var(--radius-sm);
		background: var(--canvas-float);
		border: 1px solid var(--border-default);
		box-shadow: var(--shadow-md);
		color: var(--text-secondary);
		font-size: 11px;
		font-family: var(--font-chinese);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		opacity: 0;
		pointer-events: none;
		transition: opacity var(--dur-fast) var(--ease-out);
	}

	.minimap-tick:hover .minimap-preview {
		opacity: 1;
	}

	@media (max-width: 900px) {
		/* The rail overlaps content on narrow screens; hide it there (keyboard nav
		   still works). */
		.minimap {
			display: none;
		}
	}

	/* ---- SCROLL-TO-BOTTOM (floats over the stream while reading history) ---- */
	.to-bottom {
		position: absolute;
		bottom: var(--space-4);
		left: 50%;
		transform: translateX(-50%);
		z-index: var(--z-sticky);
		display: flex;
		align-items: center;
		justify-content: center;
		width: 32px;
		height: 32px;
		border-radius: 50%;
		/* Neutral surface — NOT the lime accent (reserved for user bubble + Send). */
		background: var(--canvas-float);
		border: 1px solid var(--border-default);
		color: var(--text-secondary);
		box-shadow: var(--shadow-md);
		cursor: pointer;
		transition:
			background var(--dur-fast) var(--ease-out),
			color var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out),
			transform var(--dur-fast) var(--ease-out);
	}

	.to-bottom:hover {
		background: var(--canvas-overlay);
		color: var(--text-primary);
		border-color: var(--border-strong);
		transform: translateX(-50%) translateY(1px);
	}

	/* ---- INHERITED CONTEXT (dimmed parent history above a branch) ---- */
	/* The whole block sits back: reduced opacity + a muted wash so it reads as
	   "what came before", never competing with the live conversation. Reuses the
	   normal item classes (user bubble, markdown text) so branched history looks
	   like the real thing, just quieter. DESIGN.md §4.8. */
	.inherited {
		opacity: 0.62;
	}

	/* Live turns already carry full color; only lift the inherited text's own
	   emphasis down a notch so it stays secondary even at full markdown. */
	.inherited-text {
		color: var(--text-secondary);
	}

	/* Inherited tool call: a compact one-line trace (name + args), not the full
	   three-state ToolBlock — history doesn't need live affordances. */
	.inherited-tool {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
		font-family: var(--font-mono);
		font-size: 11.5px;
		color: var(--text-tertiary);
		min-width: 0;
	}
	.inherited-tool-name {
		color: var(--text-secondary);
		flex-shrink: 0;
	}
	.inherited-tool-args {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-tertiary);
	}

	/* Separator marking the branch point: a hairline rule with a centered mono
	   label ("resumed; continue below"). */
	.inherited-sep {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		margin: var(--space-4) 0;
		color: var(--text-tertiary);
		font-family: var(--font-mono);
		font-size: 10px;
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}
	.inherited-sep::before,
	.inherited-sep::after {
		content: '';
		flex: 1;
		height: 1px;
		background: var(--border-subtle);
	}
	.inherited-sep span {
		flex-shrink: 0;
	}

	.empty {
		color: var(--text-tertiary);
		text-align: center;
		margin-top: 25vh;
		font-size: 13px;
		font-family: var(--font-chinese);
	}

	/* Loading skeleton: conversation-shaped shimmer while the view loads.
	 *  Rows mirror the real layout (right-aligned user bubbles, full-width
	 *  text, tool cards) so the reveal feels like the content arriving, not a
	 *  spinner being replaced. Same centered measure as .conv-inner. */
	.loading-skeleton {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		margin-top: var(--space-6);
		max-width: 740px;
		margin-left: auto;
		margin-right: auto;
	}
	.sk-row {
		display: flex;
	}
	.sk-user {
		justify-content: flex-end;
	}

	/* ---- TODO DOCK (sticky above input) ---- */
	/* Sticky dock owns the conversation↔composer divider (border-top); the input
	   below drops its own top border when docked (.input-area.seamless) so there's
	   one clean line above the dock, not a stray seam between dock and input. */
	.plan-dock {
		flex-shrink: 0;
		padding: var(--space-3) var(--space-10) 0;
		background: var(--canvas-raised);
		border-top: 1px solid var(--border-subtle);
	}
	.plan-dock-inner {
		max-width: 740px;
		margin: 0 auto;
	}

	/* ---- INPUT AREA ---- */
	.input-area {
		border-top: 1px solid var(--border-subtle);
		padding: var(--space-4) var(--space-10);
		background: var(--canvas-raised);
		flex-shrink: 0;
	}

	/* Docked todo above: the dock already drew the divider, so drop ours to avoid
	   a double seam squeezed between the dock and the input box. */
	.input-area.seamless {
		border-top: none;
	}

	.input-inner {
		max-width: 740px;
		margin: 0 auto;
	}

	/* ---- PENDING QUEUE (messages sent mid-turn, awaiting flush) ---- */
	.queue {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		margin-bottom: var(--space-2);
	}

	.queue-chip {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 5px var(--space-2) 5px var(--space-3);
		border: 1px dashed var(--border-default);
		border-radius: var(--radius-md);
		background: var(--canvas-overlay);
	}

	/* Pulsing dot marks "waiting to send" — distinct from the accent Send CTA. */
	.queue-dot {
		width: 5px;
		height: 5px;
		border-radius: 50%;
		background: var(--state-running);
		flex-shrink: 0;
		animation: pulse 1.4s ease-in-out infinite;
	}

	.queue-text {
		flex: 1;
		min-width: 0;
		font-size: 12px;
		color: var(--text-secondary);
		font-family: var(--font-chinese);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.queue-cancel {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 18px;
		height: 18px;
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--text-tertiary);
		cursor: pointer;
		flex-shrink: 0;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.queue-cancel:hover {
		color: var(--state-error-text);
		background: var(--state-error-bg);
	}

	.input-box {
		background: var(--canvas-overlay);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-lg);
		/* No overflow:hidden — the config popover (absolute, anchored in the
		   actions row) must escape upward. Corners are rounded on the children
		   (textarea top, actions bottom) instead so the box still reads as one
		   rounded unit. */
		transition:
			border-color var(--dur-std) var(--ease-out),
			box-shadow var(--dur-std) var(--ease-out);
	}

	.input-box:focus-within {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 8%, transparent);
	}

	.input-field {
		width: 100%;
		padding: var(--space-3) var(--space-4);
		background: transparent;
		border: none;
		border-radius: var(--radius-lg) var(--radius-lg) 0 0;
		outline: none;
		color: var(--text-primary);
		font-family: var(--font-chinese);
		font-size: 13px;
		line-height: 1.6;
		resize: none;
		min-height: 44px;
		max-height: 120px;
	}

	.input-field::placeholder {
		color: var(--text-disabled);
	}

	/* Read-only (archived session): the box stays visible for context but reads
	 * as inert — no caret, no text selection affordance, dimmed placeholder. */
	.input-field:disabled {
		cursor: not-allowed;
		color: var(--text-tertiary);
	}

	.input-field:disabled::placeholder {
		color: var(--text-tertiary);
	}

	.input-actions {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border-top: 1px solid var(--border-subtle);
		border-radius: 0 0 var(--radius-lg) var(--radius-lg);
	}

	.input-status {
		font-size: 11px;
		color: var(--text-tertiary);
		flex: 1;
		font-family: var(--font-chinese);
	}

	.input-status .status-warn {
		color: var(--state-running-text);
	}

	/* Live turn indicator in the composer status row: amber pulsing dot +
	 *  label, matching the session-list running state (color+shape+motion). */
	.status-running {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--state-running-text);
	}
	.status-running-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--state-running);
		animation: pulse 1.4s ease-in-out infinite;
	}
	@media (prefers-reduced-motion: reduce) {
		.status-running-dot {
			animation: none;
		}
	}

	.input-btn {
		display: flex;
		align-items: center;
		gap: 5px;
		padding: 5px 10px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border-default);
		background: transparent;
		color: var(--text-secondary);
		font-size: 11.5px;
		font-weight: 450;
		cursor: pointer;
		font-family: var(--font-sans);
		transition: all var(--dur-fast) var(--ease-out);
	}

	.input-btn:hover {
		background: var(--surface-hover);
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.input-btn:active {
		transform: translateY(1px);
	}

	/* ---- SEND BUTTON (accent circular icon) ---- */
	.send-btn {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 30px;
		height: 30px;
		flex-shrink: 0;
		border: none;
		border-radius: 50%;
		background: var(--accent);
		color: var(--accent-fg);
		cursor: pointer;
		transition:
			background var(--dur-fast) var(--ease-out),
			transform var(--dur-fast) var(--ease-out),
			opacity var(--dur-fast) var(--ease-out);
	}

	.send-btn:hover:not(:disabled) {
		background: var(--accent-hover);
		/* A small lift echoing the upward "send" motion. */
		transform: translateY(-1px);
	}

	.send-btn:active:not(:disabled) {
		transform: translateY(0);
	}

	.send-btn:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}

	.send-icon {
		display: block;
	}

	/* In-flight spinner, tinted onto the accent fill. */
	.send-spinner {
		width: 13px;
		height: 13px;
		border: 1.6px solid color-mix(in srgb, var(--accent-fg) 30%, transparent);
		border-top-color: var(--accent-fg);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
	}

	.input-btn.cancel {
		color: var(--state-error-text);
		border-color: color-mix(in srgb, var(--state-error) 22%, transparent);
	}

	.input-btn.cancel:hover {
		background: var(--state-error-bg);
		border-color: color-mix(in srgb, var(--state-error) 40%, transparent);
		color: var(--state-error-text);
	}

	/* ---- DRAFT CONFIG PICKER (profile / model / workspace) ---- */
	/* The config pickers cluster: profile (config-level, lazy switch) and
	   model/effort (per-turn) as separate one-click triggers in the input
	   actions row (DESIGN.md §4.2). */
	.pickers {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		flex-shrink: 1;
		min-width: 0;
	}

	/* The per-picker trigger/popover styles live in `PickerSelect.svelte`; this
	   cluster only lays the triggers out in a row. */

	.input-hint {
		font-size: 10.5px;
		color: var(--text-disabled);
		font-family: var(--font-mono);
		margin-top: 5px;
		padding-left: 2px;
	}

	/* Pending lazy profile switch: amber (a state, not an action — no accent),
	   same quiet line as the hint it replaces. */
	.profile-note {
		color: var(--state-running-text);
		font-family: var(--font-chinese);
	}

	.profile-note strong {
		font-weight: 590;
	}

	/* Shared keyframes (pulse/spin) used by the queue dot, status dot and
	 *  send spinner in this component. */
	@keyframes pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.3;
		}
	}

	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.send-spinner {
			animation: none;
		}
		.queue-dot {
			animation: none;
		}
	}

	/* Narrow: stack the detail rail under the conversation instead of beside it,
	 * so neither column gets crushed. The grid drives both columns to one. */
	@media (max-width: 900px) {
		.session-grid {
			grid-template-columns: 1fr;
			grid-template-rows: 1fr auto;
			overflow-y: auto;
		}
	}
</style>
