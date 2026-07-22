<script lang="ts">
	import { onDestroy, onMount, tick } from 'svelte';
	import { browser } from '$app/environment';
	import { goto } from '$app/navigation';
	import { fly, fade, scale } from 'svelte/transition';
	import { Marked } from 'marked';
	import hljs from 'highlight.js/lib/common';
	import DOMPurify from 'dompurify';
	import { client } from '$lib/client';
	import type { EventSubscription } from '$lib/client-core';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
	import type { Message } from '$lib/types/Message';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import type { ProfileSummary } from '$lib/types/ProfileSummary';
	import type { ModelSummary } from '$lib/types/ModelSummary';
	import {
		apply,
		emptyState,
		type ConversationState,
		type Item,
		type PlanStep
	} from '$lib/conversation';
	import ToolBlock from '$lib/components/tools/ToolBlock.svelte';
	import type { ApprovalScope } from '$lib/types/ApprovalScope';
	import { num, statLabel, formatCost, cacheLabel, topTools } from '$lib/stats';
	import { markSeen } from '$lib/status.svelte';
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

	let convo = $state<ConversationState>(emptyState());
	let input = $state('');
	let sending = $state(false);
	// Messages the user sent while a turn was running: held here (and mirrored to
	// localStorage per session) as pending chips instead of hitting the gateway,
	// which would defer them invisibly. Flushed one-at-a-time on `turn_settled`.
	let queued = $state<QueuedMessage[]>([]);
	let error = $state<string | null>(null);
	// Draft-only session config: profile / model override / workspace, chosen
	// before the first send. Populated from the gateway when a draft opens; the
	// real session is created with these on first send (and they're read-only
	// thereafter — a session's config is immutable, doc/profile.md §5).
	let profiles = $state<ProfileSummary[]>([]);
	let models = $state<ModelSummary[]>([]);
	let selProfile = $state('');
	// Model override as `provider/model_id`; empty = use the profile default.
	let selModel = $state('');
	let cfgOpen = $state(false);
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
	// Live context-window occupancy, from the per-round `context_updated` event.
	// `tokens` is the running estimate; `window` the model's full context window
	// (0 = unknown); `threshold` the compaction fraction (drawn as a gauge tick,
	// mirroring the TUI — the gauge is tokens/window, NOT tokens/effective_limit).
	// Reset on session switch.
	let context = $state<{ tokens: number; window: number; threshold: number } | null>(null);
	// Debounce handle for per-request STATS refresh (Q2): a long turn fires many
	// RequestCompleted events; coalesce them so we don't replay the log per event.
	let summaryDebounce: ReturnType<typeof setTimeout> | undefined;
	let sub: EventSubscription | undefined;
	let streamEl = $state<HTMLElement | null>(null);
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
	// Whether we're in the initial event-replay phase (loading existing history
	// from the durable log). During replay, events arrive in rapid bursts and we
	// use instant scroll instead of `behavior: 'smooth'` to avoid the visually
	// uncomfortable rapid-scrolling animation through the entire conversation.
	let isReplaying = $state(false);
	let replayDebounce: ReturnType<typeof setTimeout> | undefined;
	/** Enter animation for stream items (DESIGN.md §3.2). The replay burst pours
	 *  history in faster than any animation, so duration collapses to 0 while
	 *  `isReplaying`; only post-replay items (a live turn's output, a freshly
	 *  sent bubble) get the 8px rise. Evaluated when each intro triggers. */
	const itemEnter = $derived(isReplaying ? { duration: 0 } : rise(8, 200));
	/** Cached conversation state per session. Preserves streaming content
	 *  (e.g. in-progress reasoning) across session switches. Live deltas are
	 *  not replayed on re-subscribe (gateway-transport.ts §reconnect), so without
	 *  this cache, switching away during a stream and back would truncate it. */
	const sessionCache = new Map<
		string,
		{ convo: ConversationState; collapsed: Record<number, boolean> }
	>();
	let prevSessionId: string | undefined;

	function toggleDetail() {
		detailOpen = !detailOpen;
		localStorage.setItem('detailOpen', detailOpen ? '1' : '0');
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
		if (streamEl) ro.observe(streamEl);
		window.addEventListener('keydown', onWindowKeydown);
		return () => {
			ro.disconnect();
			cancelAnimationFrame(measureRaf);
			window.removeEventListener('keydown', onWindowKeydown);
		};
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
		await refreshSummary(id);
		await loadInherited(id);
	}

	/** One rendered line of inherited context: the parent conversation a branched
	 *  session carries, flattened from the snapshot's `Message[]` into the few
	 *  kinds we display (system prompt is identity, not conversation, so skipped —
	 *  mirroring the TUI's seed_history). Tool results are folded onto their call. */
	type InheritedItem =
		| { kind: 'user'; text: string }
		| { kind: 'text'; text: string }
		| { kind: 'tool'; name: string; args: string; result?: string };

	/** Map a snapshot `Message[]` into `InheritedItem[]` for dimmed display.
	 *  Mirrors `src/tui/mod.rs` seed_history: System is dropped; Assistant text and
	 *  each tool call become items; a Tool result attaches to the most recent tool
	 *  item (by call id) so it renders under the call that produced it. */
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

	function subscribe(id: string) {
		sub?.close();
		// Restore cached state if available (preserves streaming content across
		// session switches); otherwise start fresh.
		const cached = sessionCache.get(id);
		if (cached) {
			convo = cached.convo;
			collapsed = cached.collapsed;
			sessionCache.delete(id);
		} else {
			convo = emptyState();
			collapsed = {};
		}
		context = null;
		// Restore this session's persisted pending queue (survives a mid-turn
		// refresh). Keyed by id, so switching sessions swaps the right queue in.
		queued = loadQueue(id);
		// A new subscription means fresh content – start auto-scrolling.
		shouldAutoScroll = true;
		// Enter replay mode: the gateway will replay committed events in rapid
		// succession. During replay we use instant scroll (no smooth animation)
		// and debounce — once events stop arriving we consider replay done.
		isReplaying = true;
		clearTimeout(replayDebounce);
		if (cached) {
			// A cached restore subscribes `since` lastSeq — there is no replay
			// burst to wait out. Close the replay window even when the gateway
			// sends nothing, otherwise every `!isReplaying` gate (summary
			// refresh, item enter animation) stays stuck off on an idle session.
			replayDebounce = setTimeout(() => {
				isReplaying = false;
			}, 300);
		}
		sub = client.subscribeEvents(
			id,
			{
				onEvent: (ev) => {
					convo = apply(convo, ev);
					if (ev.type === 'compacted') {
						// Compaction swaps the live session for a fresh one. Navigate to
						// its route; page.params.id updates → the sync $effect re-subscribes
						// and reloads meta for the new id (and the sidebar highlights it).
						// The new session's committed events replay over the fresh stream,
						// so nothing is lost in the brief gap.
						void goto(`/workspaces/${workspaceId}/sessions/${ev.new_session_id}`);
					}
					// A settled turn means the fold's aggregates changed — refresh the
					// STATS snapshot so turns/cost/tokens track the live conversation.
					if (ev.type === 'turn_settled') {
						void refreshSummary(id);
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
					// model round's usage landed, so the aggregates moved mid-turn. Debounced,
					// and skipped during history replay (loadMeta already refreshed on load).
					if (!isReplaying && ev.type === 'event') {
						const p = ev.payload;
						if ('Model' in p && 'RequestCompleted' in p.Model) {
							scheduleSummaryRefresh(id);
						}
					}
					if (isReplaying) {
						// During replay: snap to bottom instantly (no animation) to
						// avoid the uncomfortable rapid smooth-scrolling visual.
						requestAnimationFrame(() => {
							if (streamEl) streamEl.scrollTop = streamEl.scrollHeight;
						});
						// Reset the debounce timer — when events stop arriving for
						// 300 ms we consider the replay phase complete.
						clearTimeout(replayDebounce);
						replayDebounce = setTimeout(() => {
							isReplaying = false;
							// Final instant snap to ensure we're at the bottom.
							requestAnimationFrame(() => {
								if (streamEl) streamEl.scrollTop = streamEl.scrollHeight;
							});
						}, 300);
					} else if (shouldAutoScroll) {
						// Live event: smooth scroll only when the user is already at
						// (or near) the bottom. This prevents yanking them back to
						// the latest message while they're reading history.
						requestAnimationFrame(() => {
							streamEl?.scrollTo({ top: streamEl.scrollHeight, behavior: 'smooth' });
						});
					}
				},
				onError: (e) => {
					error = e instanceof Error ? e.message : String(e);
				}
			},
			cached ? convo.lastSeq : undefined
		);
	}

	$effect(() => {
		const id = sessionId;
		// Save previous session's conversation state before switching.
		// Preserves streaming content (e.g. in-progress reasoning) that would
		// otherwise be lost — live deltas are not replayed on re-subscribe.
		if (prevSessionId !== undefined && prevSessionId !== DRAFT_ID && prevSessionId !== id) {
			sessionCache.set(prevSessionId, { convo, collapsed });
		}
		prevSessionId = id;
		// Draft: show an empty conversation, don't subscribe or load meta. The
		// real session doesn't exist yet — it's created on the first send().
		if (id === DRAFT_ID) {
			sub?.close();
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

	/** Load the profile + model lists for the config picker. Fetched once
	 *  (guarded), best-effort — a failure leaves the lists empty so the picker
	 *  just offers nothing (draft falls back to gateway defaults; a live session
	 *  simply can't reconfigure). */
	async function loadConfigOptions() {
		if (profiles.length > 0 || models.length > 0) return;
		try {
			[profiles, models] = await Promise.all([client.listProfiles(), client.listModels()]);
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
		clearTimeout(replayDebounce);
		clearTimeout(summaryDebounce);
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
				// fixed by `workspaceId` (path resolved server-side, never sent), so
				// only the picker's profile/model ride along on the plain path.
				const realId = forkIntent
					? await client.forkSession(forkIntent.parentId, forkIntent.atSeq)
					: await client.createWorkspaceSession(workspaceId, {
							profile: selProfile || undefined,
							model: selModel || undefined
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
				await client.sendMessage(realId, text);
				input = '';
				await goto(`/workspaces/${workspaceId}/sessions/${realId}`);
			} else {
				await client.sendMessage(sessionId, text);
				input = '';
			}
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			sending = false;
		}
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
			// (fail loud): the message stays visible and retries on the next settle.
			queued = [next, ...queued];
			persistQueue();
			error = e instanceof Error ? e.message : String(e);
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

	// Compaction stays available programmatically; the button was removed in
	// favour of a future `/` command (see redesign plan). Keep the function so
	// the slash-command wiring can call it later.
	// eslint-disable-next-line @typescript-eslint/no-unused-vars
	async function compact() {
		try {
			await client.compact(sessionId);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	function onKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter' && !e.shiftKey) {
			e.preventDefault();
			void send();
		}
	}

	/** Markdown renderer with synchronous syntax highlighting + a language
	 *  badge on every fenced block. Built once at module load so highlight.js
	 *  language defs register a single time. `gfm` is on by default in marked
	 *  v18 — that's what enables pipe tables.
	 *
	 *  We use a custom `code` renderer (not marked-highlight) because we need the
	 *  *resolved* language for the badge: when a fence has no tag we run
	 *  `highlightAuto`, whose result carries the detected language — info the
	 *  highlight-only plugin throws away. The emitted markup is a wrapper holding
	 *  a label + the usual <pre><code>; hljs output is pre-escaped and the whole
	 *  thing is DOMPurify-sanitized downstream. */
	const md = new Marked();
	md.use({
		renderer: {
			code({ text, lang }) {
				const tag = (lang ?? '').trim().split(/\s+/)[0];
				let label: string;
				let html: string;
				if (tag && hljs.getLanguage(tag)) {
					label = tag;
					html = hljs.highlight(text, { language: tag, ignoreIllegals: true }).value;
				} else {
					const auto = hljs.highlightAuto(text);
					label = auto.language ?? 'text';
					html = auto.value;
				}
				return (
					`<div class="code-block">` +
					`<div class="code-lang">${escapeHtml(label)}</div>` +
					`<pre><code class="hljs language-${escapeHtml(label)}">${html}</code></pre>` +
					`</div>`
				);
			}
		}
	});

	function renderMarkdown(text: string): string {
		if (!browser) return escapeHtml(text);
		const raw = md.parse(text, { async: false }) as string;
		return DOMPurify.sanitize(raw);
	}

	function escapeHtml(s: string): string {
		return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
	}

	function toggleCollapse(item: Item, i: number) {
		// Flip from the currently *displayed* state, not the raw map value.
		// Auto-collapsed items have no map entry yet, so `!collapsed[i]` would
		// compute `true` (= still collapsed) on first click, needing a second
		// click to take effect. Seed from isCollapsed so one click always flips.
		collapsed = { ...collapsed, [i]: !isCollapsed(item, i) };
	}

	function isCollapsed(item: Item, i: number): boolean {
		if (i in collapsed) return collapsed[i];
		// Auto-collapse finished reasoning (tools stay open per user preference)
		if (item.kind === 'reasoning') return !item.streaming;
		// Auto-collapse a plan once every step is terminal (the work is done);
		// keep an active plan open so the running task stays visible.
		if (item.kind === 'plan') return item.steps.length > 0 && planDone(item.steps);
		return false;
	}

	/** Whether every step has reached a terminal state. An empty plan is not
	 *  "done" — it is a placeholder still being established. */
	function planDone(steps: PlanStep[]): boolean {
		return steps.length > 0 && steps.every((s) => isTerminal(s.status));
	}

	function isTerminal(status: PlanStep['status']): boolean {
		return status === 'completed' || status === 'cancelled' || status === 'blocked';
	}

	/** Resolved-step count over total, for the plan header progress. Cancelled
	 *  counts as resolved (the step was objectively unreachable and dealt with),
	 *  so the bar only stays short while a step is still pending/in_progress or
	 *  BLOCKED — i.e. a sub-100% bar signals a step is waiting on the user, not
	 *  merely cancelled. See StepStatus in `src/agent/plan.rs`. */
	function planProgress(steps: PlanStep[]): { done: number; total: number } {
		const done = steps.filter((s) => s.status === 'completed' || s.status === 'cancelled').length;
		return { done, total: steps.length };
	}

	function shortPreview(text: string): string {
		const first = text.split('\n')[0].slice(0, 60);
		return first.length < text.split('\n')[0].length ? first + '…' : first;
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

	/** Short workspace label for the INFO panel: last two path segments, full
	 *  path on hover. */
	function wsLabel(ws: string): string {
		const parts = ws.split('/').filter(Boolean);
		return parts.length > 2 ? '…/' + parts.slice(-2).join('/') : ws;
	}

	const incomplete = $derived(convo.lastSettle != null);
	// Cancel only makes sense while a turn is running (the backend ignores Cancel
	// when idle), so the button is shown only then — see ConversationState.turnRunning.
	const turnRunning = $derived(convo.turnRunning === true);

	// The seq of the FIRST user message in the view. Forking before it would
	// inherit an empty context (identical to a plain new session), so that first
	// message alone gets no fork affordance — every later user turn does.
	const firstUserSeq = $derived.by<number | null>(() => {
		for (const it of convo.items) {
			if (it.kind === 'user' && it.seq != null) return it.seq;
		}
		return null;
	});

	// The plan shown in the sticky dock: the latest committed plan card (running
	// OR finished). Once any plan exists it stays docked, so a later plan swaps in
	// place rather than the dock vanishing and a new one popping in abruptly.
	// Older plans (superseded by a newer `init`) fall back to inline history.
	// Carrying the index lets the inline render skip it — shown in one place only.
	const dockPlan = $derived.by<{ steps: PlanStep[]; index: number } | null>(() => {
		for (let i = convo.items.length - 1; i >= 0; i--) {
			const it = convo.items[i];
			// Skip streaming placeholders (empty, half-arrived) so the dock never
			// blanks between ops; the last committed card is the live plan.
			if (it.kind === 'plan' && !it.streaming && it.steps.length > 0) {
				return { steps: it.steps, index: i };
			}
		}
		return null;
	});

	// Collapsed state for the sticky dock. Defaults collapsed so the plan + input
	// don't eat a big vertical slab; the user expands to see steps. Separate from
	// the inline-item `collapsed` map (keyed by item index).
	let pinnedPlanCollapsed = $state(true);

	/** One-line label for the config trigger: the chosen profile (or "default")
	 *  and the chosen model's bare id (or "default model"). On a draft this is
	 *  what the session will be created on; on a live session it's what it runs on
	 *  now (and what a reconfiguration would change). */
	const cfgLabel = $derived.by(() => {
		const p = selProfile || 'default';
		const m = selModel ? selModel.split('/').pop() : 'default model';
		return `${p} · ${m}`;
	});

	// Reconfiguration (live sessions only): changing profile/model on an existing
	// session can't edit it in place (history is immutable), so it mints a new
	// reconfiguration session seeded with this conversation. We track the live
	// session's *current* profile/model to detect a pending change and whether a
	// reconfigure request is in flight.
	let curProfile = $state('');
	let curModel = $state('');
	let reconfiguring = $state(false);

	/** A live session has a pending config change when the picker's profile/model
	 *  differs from what the session currently runs on. Drives the Apply button. */
	const cfgDirty = $derived(!isDraft && (selProfile !== curProfile || selModel !== curModel));

	/** Apply a live config change by reconfiguring into a new session, then adopt
	 *  its id (same swap the compaction path uses). No-op on a draft (there the
	 *  choice is applied at first send instead). */
	async function applyReconfigure() {
		if (isDraft || !cfgDirty || reconfiguring) return;
		reconfiguring = true;
		error = null;
		try {
			const newId = await client.reconfigure(sessionId, {
				profile: selProfile || undefined,
				model: selModel || undefined
			});
			cfgOpen = false;
			// Navigate to the reconfigured session's route (a real goto, not a
			// shallow replaceState) so page.params.id updates — the sync $effect then
			// re-subscribes + reloads meta, and the sidebar highlights the new row.
			await goto(`/workspaces/${workspaceId}/sessions/${newId}`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			reconfiguring = false;
		}
	}

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
			<a href="/" class="topbar-back" title="返回首页" aria-label="Back to home">
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
					<span class="mono draft-hint">draft · 发送后创建</span>
				{:else}
					<button class="session-id-btn" onclick={copyId} title={copied ? '已复制!' : sessionId}>
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
					title={detailOpen ? '收起信息栏' : '展开信息栏'}
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

		<!-- CONVERSATION SCROLL -->
		<div class="conv-viewport">
			<div class="conv-scroll" bind:this={streamEl} onscroll={onStreamScroll}>
				<div class="conv-inner">
					<!-- Inherited context: the parent conversation this branched session was
				     seeded with (fork/compaction/reconfiguration), rendered dimmed above the
				     live turns so the branch shows what came before. See DESIGN.md §4.8. -->
					{#if inherited.length > 0}
						<div class="inherited" aria-label="继承的上下文">
							{#each inherited as it, k (k)}
								{#if it.kind === 'user'}
									<div class="item item-user">
										<div class="user-bubble">{it.text}</div>
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
							<div class="inherited-sep"><span>分支自此处 · inherited context above</span></div>
						</div>
					{/if}
					{#each convo.items as item, i (i)}
						{#if item.kind === 'user'}
							<div class="item item-user" data-user-anchor={i} in:fly|local={itemEnter}>
								{#if item.seq != null && !isDraft && item.seq !== firstUserSeq}
									<!-- Turn actions: low-frequency per-turn operations, anchored to
								     the user message that starts the turn. Faint by default
								     (--text-disabled), brightens when the turn is hovered. Icon +
								     tooltip only — no visible label (the glyph carries the meaning).
								     Fork is the only action today; the row is a flex container so a
								     future rating control slots in beside it. See DESIGN.md §4.7.
								     Excluded on the first user turn: branching before it inherits an
								     empty context, i.e. just a new session. -->
									<div class="turn-actions">
										<button
											class="turn-action-btn"
											onclick={() => fork(item.seq!, item.text)}
											title="从这条消息处分支（继承之前的对话，另开一个会话）"
											aria-label="Fork a new session from this message"
										>
											<!-- Original divergence glyph: a single stem splitting into
									     two branches — drawn from scratch, not a borrowed
									     git-branch icon (DESIGN §5). Round caps read as soft
									     nodes at the three tips. -->
											<svg
												class="turn-action-icon"
												viewBox="0 0 14 14"
												fill="none"
												stroke="currentColor"
												stroke-width="1.5"
												stroke-linecap="round"
												stroke-linejoin="round"
												aria-hidden="true"
											>
												<path d="M7 12V7.2" />
												<path d="M7 7.2 3.8 4" />
												<path d="M7 7.2 10.2 4" />
											</svg>
										</button>
									</div>
								{/if}
								<div class="user-bubble">{item.text}</div>
							</div>
						{:else if item.kind === 'text'}
							{#if item.text.trim()}
								<div
									class="item item-text"
									class:streaming={item.streaming}
									in:fly|local={itemEnter}
								>
									{#if browser}
										<!-- eslint-disable-next-line svelte/no-at-html-tags -->
										{@html renderMarkdown(item.text)}
									{:else}
										{item.text}
									{/if}
								</div>
							{/if}
						{:else if item.kind === 'reasoning'}
							{#if item.text.trim()}
								<div
									class="item item-reasoning"
									class:expanded={!isCollapsed(item, i)}
									in:fly|local={itemEnter}
								>
									<button
										class="reasoning-toggle"
										onclick={() => toggleCollapse(item, i)}
										aria-expanded={!isCollapsed(item, i)}
									>
										<svg
											class="reasoning-toggle-icon"
											viewBox="0 0 14 14"
											fill="none"
											stroke="currentColor"
											stroke-width="1.6"
											stroke-linecap="round"
											stroke-linejoin="round"
										>
											<polyline points="5,3 9,7 5,11" />
										</svg>
										<span class="reasoning-label">Thinking</span>
										{#if isCollapsed(item, i)}
											<span class="reasoning-preview">{shortPreview(item.text)}</span>
										{/if}
										{#if item.streaming}
											<span class="streaming-dot"></span>
										{/if}
									</button>
									{#if !isCollapsed(item, i)}
										<div class="reasoning-body">
											{#if browser}
												<!-- eslint-disable-next-line svelte/no-at-html-tags -->
												{@html renderMarkdown(item.text)}
											{:else}
												{item.text}
											{/if}
										</div>
									{/if}
								</div>
							{/if}
						{:else if item.kind === 'tool'}
							<div class="item" in:fly|local={itemEnter}>
								<ToolBlock {item} fileCache={convo.fileCache} onDecide={decideApproval} />
							</div>
						{:else if item.kind === 'plan'}
							<!-- Streaming placeholders (item.streaming) render nothing: a flashing
					     inline "planning…" card on every plan op is pure eye-strain, and
					     the dock already shows the live plan. The docked card is shown in
					     the dock, so skip it here too — only committed history cards render. -->
							{#if !item.streaming && i !== dockPlan?.index}
								{@const prog = planProgress(item.steps)}
								<div class="item" in:fly|local={itemEnter}>
									<div
										class="plan-card"
										class:expanded={!isCollapsed(item, i)}
										class:done={planDone(item.steps)}
									>
										<button
											class="plan-head"
											onclick={() => toggleCollapse(item, i)}
											aria-expanded={!isCollapsed(item, i)}
										>
											<svg
												class="plan-icon"
												viewBox="0 0 14 14"
												fill="none"
												stroke="currentColor"
												stroke-width="1.5"
												stroke-linecap="round"
												stroke-linejoin="round"
											>
												<path d="M3 3h8M3 7h8M3 11h5" />
											</svg>
											<span class="plan-title">Plan</span>
											<span class="plan-progress">{prog.done}/{prog.total}</span>
											<span class="plan-track"
												><span
													class="plan-bar"
													style="width: {prog.total ? (prog.done / prog.total) * 100 : 0}%"
												></span></span
											>
											<svg
												class="plan-chevron"
												viewBox="0 0 12 12"
												fill="none"
												stroke="currentColor"
												stroke-width="1.6"
												stroke-linecap="round"
												stroke-linejoin="round"
											>
												<polyline points="4,2 8,6 4,10" />
											</svg>
										</button>
										{#if !isCollapsed(item, i)}
											<ol class="plan-steps">
												{#each item.steps as step (step.id)}
													<li class="plan-step" data-status={step.status}>
														<span class="plan-step-mark" aria-hidden="true">
															{#if step.status === 'completed'}
																<svg
																	viewBox="0 0 12 12"
																	fill="none"
																	stroke="currentColor"
																	stroke-width="2"
																	stroke-linecap="round"
																	stroke-linejoin="round"
																	><polyline points="2.5,6.5 5,9 9.5,3.5" /></svg
																>
															{:else if step.status === 'in_progress'}
																<span class="plan-spinner"></span>
															{:else if step.status === 'cancelled'}
																<svg
																	viewBox="0 0 12 12"
																	fill="none"
																	stroke="currentColor"
																	stroke-width="1.8"
																	stroke-linecap="round"
																	><line x1="3" y1="3" x2="9" y2="9" /><line
																		x1="9"
																		y1="3"
																		x2="3"
																		y2="9"
																	/></svg
																>
															{:else if step.status === 'blocked'}
																<svg
																	viewBox="0 0 12 12"
																	fill="none"
																	stroke="currentColor"
																	stroke-width="1.6"
																	><circle cx="6" cy="6" r="4.2" /><line
																		x1="3"
																		y1="3"
																		x2="9"
																		y2="9"
																		stroke-linecap="round"
																	/></svg
																>
															{:else}
																<span class="plan-dot"></span>
															{/if}
														</span>
														<span class="plan-step-body">
															<span class="plan-step-text">{step.content}</span>
															{#if step.reason}
																<span class="plan-step-reason">{step.reason}</span>
															{/if}
														</span>
													</li>
												{/each}
											</ol>
										{/if}
									</div>
								</div>
							{/if}
						{:else if item.kind === 'error'}
							<div class="item item-error" in:fly|local={itemEnter}>{item.message}</div>
						{:else if item.kind === 'notice'}
							<div class="item item-notice" in:fly|local={itemEnter}>{item.message}</div>
						{/if}
					{/each}

					{#if convo.items.length === 0}
						<p class="empty">{isDraft ? '输入消息，开始一段新对话' : '发送消息开始对话'}</p>
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
					title="回到底部"
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

		<!-- ACTIVE PLAN (sticky above input) — the latest plan stays docked (running
	     or done) so a later plan swaps in place instead of popping in abruptly.
	     Default collapsed to spare vertical space; the dock carries the divider
	     line so it reads as one zone with the input below. -->
		{#if dockPlan}
			{@const prog = planProgress(dockPlan.steps)}
			<div class="plan-dock">
				<div class="plan-dock-inner">
					<div class="plan-card pinned" class:expanded={!pinnedPlanCollapsed}>
						<button
							class="plan-head"
							onclick={() => (pinnedPlanCollapsed = !pinnedPlanCollapsed)}
							aria-expanded={!pinnedPlanCollapsed}
						>
							<svg
								class="plan-icon"
								viewBox="0 0 14 14"
								fill="none"
								stroke="currentColor"
								stroke-width="1.5"
								stroke-linecap="round"
								stroke-linejoin="round"
							>
								<path d="M3 3h8M3 7h8M3 11h5" />
							</svg>
							<span class="plan-title">Plan</span>
							<span class="plan-progress">{prog.done}/{prog.total}</span>
							<span class="plan-track"
								><span
									class="plan-bar"
									style="width: {prog.total ? (prog.done / prog.total) * 100 : 0}%"
								></span></span
							>
							<svg
								class="plan-chevron"
								viewBox="0 0 12 12"
								fill="none"
								stroke="currentColor"
								stroke-width="1.6"
								stroke-linecap="round"
								stroke-linejoin="round"
							>
								<polyline points="4,2 8,6 4,10" />
							</svg>
						</button>
						{#if !pinnedPlanCollapsed}
							<ol class="plan-steps">
								{#each dockPlan.steps as step (step.id)}
									<li class="plan-step" data-status={step.status}>
										<span class="plan-step-mark" aria-hidden="true">
											{#if step.status === 'completed'}
												<svg
													viewBox="0 0 12 12"
													fill="none"
													stroke="currentColor"
													stroke-width="2"
													stroke-linecap="round"
													stroke-linejoin="round"><polyline points="2.5,6.5 5,9 9.5,3.5" /></svg
												>
											{:else if step.status === 'in_progress'}
												<span class="plan-spinner"></span>
											{:else if step.status === 'cancelled'}
												<svg
													viewBox="0 0 12 12"
													fill="none"
													stroke="currentColor"
													stroke-width="1.8"
													stroke-linecap="round"
													><line x1="3" y1="3" x2="9" y2="9" /><line
														x1="9"
														y1="3"
														x2="3"
														y2="9"
													/></svg
												>
											{:else if step.status === 'blocked'}
												<svg
													viewBox="0 0 12 12"
													fill="none"
													stroke="currentColor"
													stroke-width="1.6"
													><circle cx="6" cy="6" r="4.2" /><line
														x1="3"
														y1="3"
														x2="9"
														y2="9"
														stroke-linecap="round"
													/></svg
												>
											{:else}
												<span class="plan-dot"></span>
											{/if}
										</span>
										<span class="plan-step-body">
											<span class="plan-step-text">{step.content}</span>
											{#if step.reason}
												<span class="plan-step-reason">{step.reason}</span>
											{/if}
										</span>
									</li>
								{/each}
							</ol>
						{/if}
					</div>
				</div>
			</div>
		{/if}

		<!-- INPUT AREA -->
		<div class="input-area" class:seamless={dockPlan}>
			<div class="input-inner">
				{#if queued.length > 0}
					<!-- Pending queue: messages the user sent while a turn was running.
					     They flush one-at-a-time on each settle; until then each is a
					     cancellable chip (× drops it back into the input to edit/resend). -->
					<div class="queue" aria-label="待发送消息">
						{#each queued as item (item.id)}
							<div class="queue-chip" transition:scale={pop(120)}>
								<span class="queue-dot" aria-hidden="true"></span>
								<span class="queue-text" title={item.text}>{item.text}</span>
								<button
									class="queue-cancel"
									onclick={() => cancelQueued(item)}
									title="取消并放回输入框"
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
							? '会话已归档，仅供查看'
							: '输入消息… Enter 发送，Shift+Enter 换行'}
						rows="2"
					></textarea>
					<div class="input-actions">
						<span class="input-status">
							{#if incomplete}<span class="status-warn">Turn incomplete</span>{/if}
						</span>
						<div class="cfg">
							<button
								class="input-btn cfg-trigger"
								class:on={cfgOpen}
								onclick={() => (cfgOpen = !cfgOpen)}
								title={isDraft
									? '选择 profile / 模型 / 工作区（仅本次会话）'
									: '查看 / 切换 profile / 模型（切换会基于当前对话开启一个新会话）'}
								aria-expanded={cfgOpen}
							>
								<svg
									width="12"
									height="12"
									viewBox="0 0 14 14"
									fill="none"
									stroke="currentColor"
									stroke-width="1.4"
									stroke-linecap="round"
									stroke-linejoin="round"
								>
									<circle cx="7" cy="7" r="2.2" />
									<path
										d="M7 1.2v1.6M7 11.2v1.6M1.2 7h1.6M11.2 7h1.6M2.9 2.9l1.1 1.1M10 10l1.1 1.1M11.1 2.9 10 4M4 10l-1.1 1.1"
									/>
								</svg>
								<span class="cfg-label">{cfgLabel}</span>
							</button>
							{#if cfgOpen}
								<div class="cfg-popover" transition:fly={rise(6)}>
									<label class="cfg-field">
										<span class="cfg-key">Profile</span>
										<select class="cfg-select" bind:value={selProfile}>
											<option value="">默认</option>
											{#each profiles as p (p.name)}
												<option value={p.name}
													>{p.name}{p.description ? ` — ${p.description}` : ''}</option
												>
											{/each}
										</select>
									</label>
									<label class="cfg-field">
										<span class="cfg-key">Model</span>
										<select class="cfg-select" bind:value={selModel}>
											<option value="">默认（按 profile）</option>
											{#each models as m (`${m.provider}/${m.model_id}`)}
												<option value={`${m.provider}/${m.model_id}`}
													>{m.model_id} · {m.provider}</option
												>
											{/each}
										</select>
									</label>
									{#if !isDraft}
										<!-- Live session: a profile/model change can't edit in place; it
									     opens a new session seeded with this conversation. -->
										<div class="cfg-foot">
											<span class="cfg-hint">切换将基于当前对话开启新会话</span>
											<button
												class="input-btn primary cfg-apply"
												disabled={!cfgDirty || reconfiguring}
												onclick={applyReconfigure}
											>
												{reconfiguring ? '切换中…' : '切换'}
											</button>
										</div>
									{/if}
								</div>
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
								? '会话已归档，仅供查看'
								: !isDraft && turnRunning
									? '排队发送（当前回合结束后自动发出）· Enter'
									: '发送 · Enter'}
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
				<div class="input-hint">Type / for commands</div>
			</div>
		</div>
	</div>

	{#if !isDraft}
		<aside class="detail">
			<!-- INFO: config-layer session context (moved off the global sidebar) -->
			<section class="detail-section">
				<div class="detail-label">Info</div>
				{#if meta?.workspace}
					<div class="kv">
						<div class="kv-key">Workspace</div>
						<div class="kv-val" title={meta.workspace}>{wsLabel(meta.workspace)}</div>
					</div>
				{/if}
				{#if runtime && runtime.env.length > 0}
					<div class="kv">
						<div class="kv-key">Env</div>
						<div class="kv-val" title={runtime.env.join(' · ')}>{runtime.env.join(' · ')}</div>
					</div>
				{/if}
				{#if runtime}
					<div class="kv">
						<div class="kv-key">Model</div>
						<div class="kv-val" title={`${runtime.provider} · ${runtime.model}`}>
							{runtime.model}
						</div>
					</div>
				{/if}
				{#if divergent.length > 0}
					<div class="kv warn">
						<div class="kv-key warn-key">⚠ Runtime</div>
						<div
							class="kv-val warn-val"
							title={`runtime used ${divergent.join(', ')}, configured ${runtime?.model}`}
						>
							{divergent.join(' · ')} ≠ {runtime?.model}
						</div>
					</div>
				{/if}
				{#if meta?.profile_id}
					<div class="kv">
						<div class="kv-key">Profile</div>
						<div class="kv-val">{meta.profile_id}</div>
					</div>
				{/if}
			</section>

			<!-- CONTEXT: live per-round window occupancy (context_updated event). Its
		     own section (driven by live events, not the summary endpoint) so it
		     shows mid-turn even before the first summary snapshot loads. -->
			{#if context}
				{@const pct = context.window
					? Math.min(100, (context.tokens / context.window) * 100)
					: null}
				{@const overThreshold = pct !== null && pct / 100 >= context.threshold}
				<section class="detail-section">
					<div class="detail-label">Context</div>
					<div class="ctx">
						<div class="ctx-nums">
							<span class="ctx-val">{context.tokens.toLocaleString()}</span>
							{#if context.window}
								<span class="ctx-limit">/ {context.window.toLocaleString()}</span>
							{/if}
						</div>
						{#if pct !== null}
							<div
								class="ctx-track"
								title={`${context.tokens.toLocaleString()} / ${context.window.toLocaleString()} tokens · compaction at ${(context.threshold * 100).toFixed(0)}%`}
							>
								<span class="ctx-fill" class:warn={overThreshold} style="width: {pct}%"></span>
								<!-- compaction-threshold tick, mirroring the TUI gauge marker -->
								<span class="ctx-tick" style="left: {context.threshold * 100}%"></span>
							</div>
							<span class="ctx-pct" class:warn={overThreshold}>{pct.toFixed(0)}%</span>
						{:else}
							<span class="ctx-pct unpriced">window unknown</span>
						{/if}
					</div>
				</section>
			{/if}

			<!-- STATS: folded summary snapshot, refreshed on each settled turn -->
			{#if summary}
				{@const s = summary}
				{@const tools = topTools(s, 6)}
				<section class="detail-section">
					<div class="detail-label">Stats</div>
					<div class="stat-grid">
						<div class="stat">
							<span class="stat-value">{s.total_turns}</span>
							<span class="stat-key">{statLabel.turns(s.total_turns)}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{s.total_model_requests}</span>
							<span class="stat-key">{statLabel.reqs(s.total_model_requests)}</span>
						</div>
						<div class="stat">
							<span class="stat-value">
								{s.total_tool_calls}{#if s.total_tool_failures > 0}<span class="stat-fail"
										>/{s.total_tool_failures}✗</span
									>{/if}
							</span>
							<span class="stat-key">{statLabel.toolCalls(s.total_tool_calls)}</span>
						</div>
						<div class="stat">
							<span class="stat-value cost" class:unpriced={s.cost_usd == null}
								>{formatCost(s)}</span
							>
							<span class="stat-key">{statLabel.cost}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{num(s.total_input_tokens).toLocaleString()}</span>
							<span class="stat-key">{statLabel.inTok}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{num(s.total_output_tokens).toLocaleString()}</span>
							<span class="stat-key">{statLabel.outTok}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{cacheLabel(s)}</span>
							<span class="stat-key">{statLabel.cache}</span>
						</div>
					</div>
				</section>

				{#if tools.length > 0}
					<section class="detail-section">
						<div class="detail-label">Tool usage</div>
						<ul class="bars">
							{#each tools as t (t.tool)}
								<li class="bar-row">
									<span class="bar-label" title={t.tool}>{t.tool}</span>
									<span class="bar-track"
										><span class="bar-fill" style="width: {t.pct}%"></span></span
									>
									<span class="bar-count">{t.count}</span>
								</li>
							{/each}
						</ul>
					</section>
				{/if}
			{/if}
		</aside>
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
	.session-grid.no-detail .detail {
		display: none;
	}

	.conv-page {
		display: flex;
		flex-direction: column;
		height: 100%;
		overflow: hidden;
		min-width: 0;
	}

	/* ---- DETAIL RAIL (INFO + STATS + tool usage) ---- */
	.detail {
		height: 100%;
		overflow-y: auto;
		border-left: 1px solid var(--border-subtle);
		background: var(--canvas-raised);
		padding: var(--space-5) var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-5);
	}

	.detail-section {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.detail-label {
		font-size: 10.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.07em;
		text-transform: uppercase;
	}

	.kv {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.kv-key {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
		line-height: 1;
	}

	.kv-val {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-secondary);
		line-height: 1.4;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 100%;
	}

	/* Divergence marker: runtime model ≠ configured model (fail-loud, B4) */
	.warn-key {
		color: var(--state-error-text);
	}
	.warn-val {
		color: var(--state-error-text);
		white-space: normal;
	}

	.stat-grid {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: var(--space-3) var(--space-4);
	}

	.stat {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}

	.stat-value {
		font-family: var(--font-mono);
		font-size: 14px;
		font-weight: 500;
		color: var(--text-primary);
		font-variant-numeric: tabular-nums;
		line-height: 1.2;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.stat-value.cost {
		color: var(--accent-ink);
	}
	.stat-value.cost.unpriced {
		color: var(--text-tertiary);
		font-size: 12px;
	}

	.stat-fail {
		color: var(--state-error-text);
		font-size: 11px;
	}

	.stat-key {
		font-size: 9.5px;
		font-weight: 510;
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--text-tertiary);
	}

	.bars {
		list-style: none;
		display: grid;
		gap: 6px;
	}

	.bar-row {
		display: grid;
		grid-template-columns: minmax(48px, 84px) 1fr auto;
		align-items: center;
		gap: var(--space-2);
	}

	.bar-label {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-secondary);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.bar-track {
		background: var(--canvas-float);
		border-radius: var(--radius-sm);
		height: 6px;
		overflow: hidden;
	}

	.bar-fill {
		display: block;
		height: 100%;
		/* Muted, not accent: dense data, not the screen's one CTA. */
		background: var(--text-tertiary);
		border-radius: var(--radius-sm);
		transition: width var(--dur-std) var(--ease-out);
	}

	.bar-count {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
		min-width: 2ch;
		text-align: right;
	}

	/* ---- CONTEXT (live per-round window occupancy) ---- */
	.ctx {
		display: grid;
		grid-template-columns: 1fr auto;
		align-items: center;
		gap: var(--space-2) var(--space-3);
	}
	.ctx-nums {
		font-family: var(--font-mono);
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
	}
	.ctx-val {
		font-size: 14px;
		font-weight: 500;
		color: var(--text-primary);
	}
	.ctx-limit {
		font-size: 12px;
		color: var(--text-tertiary);
	}
	.ctx-track {
		grid-column: 1 / -1;
		order: 3;
		position: relative;
		background: var(--canvas-float);
		border-radius: var(--radius-sm);
		height: 6px;
		overflow: hidden;
	}
	.ctx-fill {
		display: block;
		height: 100%;
		background: var(--text-tertiary);
		border-radius: var(--radius-sm);
		transition: width var(--dur-std) var(--ease-out);
	}
	.ctx-fill.warn {
		background: var(--state-error-text);
	}
	/* Compaction-threshold marker (TUI gauge tick equivalent). */
	.ctx-tick {
		position: absolute;
		top: 0;
		bottom: 0;
		width: 1px;
		background: var(--text-secondary);
		transform: translateX(-0.5px);
	}
	.ctx-pct {
		font-family: var(--font-mono);
		font-size: 12px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
		text-align: right;
	}
	.ctx-pct.warn {
		color: var(--state-error-text);
	}
	.ctx-pct.unpriced {
		font-size: 11px;
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
	   label, echoing the TUI's "resumed; continue below" divider. */
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

	.item {
		margin-bottom: var(--space-4);
	}

	.empty {
		color: var(--text-tertiary);
		text-align: center;
		margin-top: 25vh;
		font-size: 13px;
		font-family: var(--font-chinese);
	}

	/* ---- USER ---- */
	.item-user {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: var(--space-1);
	}

	.user-bubble {
		max-width: 560px;
		background: var(--user-bg);
		border: 1px solid var(--user-border);
		border-radius: var(--radius-lg);
		padding: var(--space-3) var(--space-4);
		font-size: 13px;
		color: var(--text-primary);
		line-height: 1.6;
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}

	/* ---- TURN ACTIONS (fork today; reserved slot for a future rating control,
	   DESIGN.md §4.7) ---- */
	/* Faint, low-frequency affordance: dim by default so it never competes with
	   the conversation, brightening only when its turn is hovered. Icon-only —
	   the meaning rides on the glyph + tooltip, no visible label. */
	.turn-actions {
		display: flex;
		align-items: center;
		opacity: 0.4;
		transition: opacity var(--dur-fast) var(--ease-out);
	}

	.item-user:hover .turn-actions,
	.turn-actions:focus-within {
		opacity: 1;
	}

	.turn-action-btn {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 20px;
		padding: 0;
		border: 1px solid transparent;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--text-disabled);
		cursor: pointer;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
	}

	.turn-action-btn:hover {
		color: var(--text-secondary);
		background: var(--canvas-float);
		border-color: var(--border-default);
	}

	.turn-action-icon {
		width: 12px;
		height: 12px;
		flex-shrink: 0;
	}

	@media (prefers-reduced-motion: reduce) {
		.turn-actions,
		.turn-action-btn {
			transition: none;
		}
	}

	/* ---- AGENT TEXT ---- */
	.item-text {
		font-size: 13.5px;
		line-height: 1.75;
		color: var(--text-primary);
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}

	.item-text :global(p) {
		margin-bottom: var(--space-3);
	}
	.item-text :global(p:last-child) {
		margin-bottom: 0;
	}
	.item-text :global(h1),
	.item-text :global(h2),
	.item-text :global(h3) {
		font-weight: 600;
		margin: 1em 0 0.5em;
		color: var(--text-primary);
		line-height: 1.3;
	}
	.item-text :global(strong) {
		font-weight: 600;
		color: var(--text-primary);
	}
	.item-text :global(code) {
		font-family: var(--font-mono);
		font-size: 12px;
		background: var(--canvas-float);
		color: var(--syntax-str);
		padding: 1px 5px;
		border-radius: 3px;
		border: 1px solid var(--border-subtle);
	}
	.item-text :global(.code-block) {
		margin: var(--space-3) 0;
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		overflow: hidden;
		background: var(--canvas-float);
	}
	.item-text :global(.code-lang) {
		font-family: var(--font-mono);
		font-size: 10px;
		font-weight: 510;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-tertiary);
		padding: 5px var(--space-3);
		background: var(--canvas-overlay);
		border-bottom: 1px solid var(--border-subtle);
		user-select: none;
	}
	.item-text :global(.code-block pre) {
		margin: 0;
		padding: var(--space-3);
		background: none;
		border: none;
		border-radius: 0;
		overflow-x: auto;
	}
	.item-text :global(pre code) {
		background: none;
		border: none;
		padding: 0;
		color: var(--text-secondary);
	}

	/* ---- CODE SYNTAX HIGHLIGHT (hljs token → design token) ---- */
	.item-text :global(.hljs-comment),
	.item-text :global(.hljs-quote) {
		color: var(--syntax-comment);
		font-style: italic;
	}
	.item-text :global(.hljs-keyword),
	.item-text :global(.hljs-selector-tag),
	.item-text :global(.hljs-literal),
	.item-text :global(.hljs-section),
	.item-text :global(.hljs-doctag),
	.item-text :global(.hljs-name) {
		color: var(--syntax-keyword);
	}
	.item-text :global(.hljs-string),
	.item-text :global(.hljs-regexp),
	.item-text :global(.hljs-meta .hljs-string) {
		color: var(--syntax-str);
	}
	.item-text :global(.hljs-number),
	.item-text :global(.hljs-bullet) {
		color: var(--syntax-num);
	}
	.item-text :global(.hljs-title),
	.item-text :global(.hljs-title.function_),
	.item-text :global(.hljs-function .hljs-title),
	.item-text :global(.hljs-built_in) {
		color: var(--syntax-fn);
	}
	.item-text :global(.hljs-type),
	.item-text :global(.hljs-class .hljs-title),
	.item-text :global(.hljs-attr),
	.item-text :global(.hljs-attribute),
	.item-text :global(.hljs-property) {
		color: var(--syntax-type);
	}
	.item-text :global(.hljs-variable),
	.item-text :global(.hljs-template-variable),
	.item-text :global(.hljs-symbol) {
		color: var(--syntax-key);
	}
	.item-text :global(.hljs-emphasis) {
		font-style: italic;
	}
	.item-text :global(.hljs-strong) {
		font-weight: 600;
	}

	/* ---- TABLES (GFM) ---- */
	.item-text :global(table) {
		border-collapse: collapse;
		width: max-content;
		max-width: 100%;
		margin: var(--space-3) 0;
		font-size: 12.5px;
		font-family: var(--font-sans);
		display: block;
		overflow-x: auto;
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
	}
	.item-text :global(th),
	.item-text :global(td) {
		border-right: 1px solid var(--border-subtle);
		border-bottom: 1px solid var(--border-subtle);
		padding: var(--space-2) var(--space-3);
		text-align: left;
		vertical-align: top;
		line-height: 1.5;
	}
	.item-text :global(tr > th:last-child),
	.item-text :global(tr > td:last-child) {
		border-right: none;
	}
	.item-text :global(tbody tr:last-child td) {
		border-bottom: none;
	}
	.item-text :global(thead th) {
		color: var(--text-primary);
		font-weight: 590;
		font-size: 11px;
		letter-spacing: 0.02em;
		border-bottom: 1px solid var(--border-default);
	}
	.item-text :global(tbody td) {
		color: var(--text-secondary);
	}
	.item-text :global(ol),
	.item-text :global(ul) {
		padding-left: var(--space-5);
		margin-bottom: var(--space-3);
	}
	.item-text :global(li) {
		margin-bottom: var(--space-1);
		padding-left: var(--space-1);
	}
	.item-text :global(li::marker) {
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
	}
	.item-text :global(a) {
		color: var(--accent-ink);
		text-decoration: none;
		border-bottom: 1px solid color-mix(in srgb, var(--accent) 30%, transparent);
		transition: border-color var(--dur-fast);
	}
	.item-text :global(a:hover) {
		border-color: var(--accent);
	}
	.item-text :global(blockquote) {
		border-left: 2px solid var(--border-strong);
		padding-left: var(--space-3);
		color: var(--text-secondary);
		margin: var(--space-3) 0;
	}

	/* Streaming cursor on the live text item */
	.item-text.streaming::after {
		content: '';
		display: inline-block;
		width: 2px;
		height: 1em;
		background: var(--accent);
		vertical-align: text-bottom;
		margin-left: 2px;
		border-radius: 1px;
		animation: cursor-blink 1.1s step-end infinite;
	}

	@keyframes cursor-blink {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0;
		}
	}

	/* ---- REASONING ---- */
	.reasoning-toggle {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-md);
		border: 1px solid var(--reasoning-border);
		background: var(--reasoning-bg);
		cursor: pointer;
		transition:
			border-color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
		width: 100%;
		text-align: left;
	}

	.reasoning-toggle:hover {
		border-color: color-mix(in srgb, var(--reasoning-text) 40%, transparent);
	}

	.reasoning-toggle-icon {
		width: 14px;
		height: 14px;
		flex-shrink: 0;
		color: var(--reasoning-text);
		transition: transform var(--dur-std) var(--ease-out);
	}

	.item-reasoning.expanded .reasoning-toggle-icon {
		transform: rotate(90deg);
	}

	.reasoning-label {
		font-size: 10.5px;
		font-weight: 510;
		color: var(--reasoning-text);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		flex-shrink: 0;
	}

	.reasoning-preview {
		font-size: 12px;
		color: var(--text-tertiary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		flex: 1;
		font-family: var(--font-chinese);
	}

	.reasoning-body {
		padding: var(--space-3) var(--space-3) var(--space-2);
		margin-top: 2px;
		margin-left: var(--space-3);
		border-left: 2px solid var(--reasoning-border);
		font-size: 12.5px;
		color: var(--text-tertiary);
		line-height: 1.7;
		font-family: var(--font-chinese);
		text-wrap: pretty;
	}
	.reasoning-body :global(p) {
		margin-bottom: var(--space-2);
	}
	.reasoning-body :global(p:last-child) {
		margin-bottom: 0;
	}
	.reasoning-body :global(ol),
	.reasoning-body :global(ul) {
		padding-left: var(--space-5);
		margin-bottom: var(--space-2);
	}
	.reasoning-body :global(li) {
		margin-bottom: 2px;
	}
	.reasoning-body :global(li::marker) {
		color: var(--text-disabled);
		font-variant-numeric: tabular-nums;
	}
	.reasoning-body :global(code) {
		font-family: var(--font-mono);
		font-size: 11.5px;
		background: var(--canvas-float);
		padding: 1px 4px;
		border-radius: 3px;
	}

	/* ---- ERROR / NOTICE ---- */
	.item-error {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		border: 1px solid color-mix(in srgb, var(--state-error) 30%, transparent);
		border-radius: var(--radius-md);
		padding: var(--space-3) var(--space-4);
		font-size: 12.5px;
	}

	.item-notice {
		color: var(--text-tertiary);
		font-size: 12px;
		font-style: italic;
		text-align: center;
		padding: var(--space-2) var(--space-4);
		font-family: var(--font-chinese);
	}

	/* ---- PLAN CARD (inline checklist + sticky dock) ---- */
	/* Neutral surface, tool-block family — the indigo accent is rationed to the
	   Plan label/icon only (like reasoning's indigo label), so the card blends
	   into the conversation and the input dock rather than shouting. */
	.plan-card {
		border-radius: var(--radius-md);
		overflow: hidden;
		border: 1px solid var(--border-subtle);
		background: var(--canvas-overlay);
		transition: border-color var(--dur-std) var(--ease-out);
	}

	/* Sticky dock variant: matches the input box — same radius/surface/border so
	   the running plan reads as part of the composer zone. */
	.plan-card.pinned {
		border-radius: var(--radius-lg);
		border-color: var(--border-default);
	}

	.plan-head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 7px var(--space-3);
		width: 100%;
		text-align: left;
		background: transparent;
		cursor: pointer;
		user-select: none;
		transition: background var(--dur-fast) var(--ease-out);
	}

	button.plan-head:hover {
		background: color-mix(in srgb, var(--plan-accent) 8%, transparent);
	}

	.plan-icon {
		width: 13px;
		height: 13px;
		flex-shrink: 0;
		color: var(--plan-accent);
	}

	.plan-title {
		font-size: 10.5px;
		font-weight: 510;
		color: var(--plan-accent);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		flex-shrink: 0;
	}

	.plan-progress {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
		flex-shrink: 0;
	}

	/* Progress track: inline/dock use remaining flex width in the head row. */
	.plan-track {
		flex: 1;
		height: 4px;
		background: var(--canvas-float);
		border-radius: var(--radius-sm);
		overflow: hidden;
	}

	.plan-bar {
		display: block;
		height: 100%;
		background: var(--plan-accent);
		border-radius: var(--radius-sm);
		transition: width var(--dur-std) var(--ease-out);
	}

	.plan-chevron {
		width: 12px;
		height: 12px;
		color: var(--text-tertiary);
		transition: transform var(--dur-std) var(--ease-out);
		flex-shrink: 0;
	}
	.plan-card.expanded .plan-chevron {
		transform: rotate(90deg);
	}

	.plan-steps {
		list-style: none;
		padding: var(--space-2) var(--space-3) var(--space-3);
		margin: 0;
		display: grid;
		gap: var(--space-1);
		border-top: 1px solid var(--border-subtle);
	}

	.plan-step {
		display: grid;
		grid-template-columns: 14px 1fr;
		gap: var(--space-2);
		align-items: start;
		padding: 3px 0;
	}

	.plan-step-mark {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 14px;
		height: 16px; /* align icon to the first text line */
		flex-shrink: 0;
		color: var(--text-tertiary);
	}
	.plan-step-mark svg {
		width: 12px;
		height: 12px;
	}

	.plan-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		border: 1.5px solid var(--text-tertiary);
	}

	/* Per-status colours: in_progress=running amber, completed=done green,
	   blocked=error red (needs user), cancelled=muted+struck. */
	.plan-step[data-status='in_progress'] .plan-step-mark {
		color: var(--state-running);
	}
	.plan-step[data-status='completed'] .plan-step-mark {
		color: var(--state-done);
	}
	.plan-step[data-status='blocked'] .plan-step-mark {
		color: var(--state-error);
	}
	.plan-step[data-status='cancelled'] .plan-step-mark {
		color: var(--text-disabled);
	}

	.plan-step-body {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}

	.plan-step-text {
		font-size: 12.5px;
		line-height: 1.45;
		color: var(--text-secondary);
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}
	.plan-step[data-status='in_progress'] .plan-step-text {
		color: var(--text-primary);
		font-weight: 500;
	}
	.plan-step[data-status='completed'] .plan-step-text {
		color: var(--text-tertiary);
	}
	.plan-step[data-status='cancelled'] .plan-step-text {
		color: var(--text-disabled);
		text-decoration: line-through;
	}

	.plan-step-reason {
		font-size: 11px;
		line-height: 1.4;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}
	.plan-step[data-status='blocked'] .plan-step-reason {
		color: var(--state-error-text);
	}

	.plan-spinner {
		width: 10px;
		height: 10px;
		border: 1.5px solid color-mix(in srgb, var(--state-running) 25%, transparent);
		border-top-color: var(--state-running);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
	}

	/* Sticky dock: pins the active plan above the input, sharing the composer's
	   width + horizontal padding so it lines up with the input box. */
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

	/* Streaming dot (reasoning / tool live indicator) */
	.streaming-dot {
		display: inline-block;
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--accent);
		animation: pulse 1s ease-in-out infinite;
		flex-shrink: 0;
	}

	@keyframes pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.3;
		}
	}

	/* Spinner rotation. Referenced by .plan-spinner and .send-spinner; scoped to
	   this component like the other keyframes above. */
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	/* ---- INPUT AREA ---- */
	.input-area {
		border-top: 1px solid var(--border-subtle);
		padding: var(--space-4) var(--space-10);
		background: var(--canvas-raised);
		flex-shrink: 0;
	}

	/* Docked plan above: the dock already drew the divider, so drop ours to avoid
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

	.input-btn.primary {
		background: var(--accent);
		color: var(--accent-fg);
		border-color: transparent;
		font-weight: 590;
	}

	.input-btn.primary:hover {
		background: var(--accent-hover);
		color: var(--accent-fg);
	}

	.input-btn.primary:disabled {
		opacity: 0.55;
		cursor: not-allowed;
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
	/* Anchors the popover; lives in the input-actions row just left of Send so the
	   pre-send config sits in the composer cluster (the space above the input is
	   taken by the plan dock). Draft-only — gone once the session is real. */
	.cfg {
		position: relative;
		display: flex;
		flex-shrink: 0;
	}

	.cfg-trigger {
		max-width: 220px;
	}

	.cfg-trigger.on {
		background: var(--surface-hover);
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.cfg-label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--font-mono);
		font-size: 11px;
	}

	/* Opens upward (the input sits at the screen bottom). Right-aligned to the
	   trigger so it stays within the composer width. */
	.cfg-popover {
		position: absolute;
		bottom: calc(100% + 6px);
		right: 0;
		z-index: 20;
		width: 300px;
		max-width: min(300px, 80vw);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		padding: var(--space-3) var(--space-4);
		background: var(--canvas-overlay);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-md, 0 8px 24px rgba(0, 0, 0, 0.18));
	}

	.cfg-field {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}

	.cfg-key {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.cfg-select {
		width: 100%;
		padding: 5px 8px;
		background: var(--canvas-base);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		color: var(--text-primary);
		font-family: var(--font-mono);
		font-size: 12px;
		outline: none;
	}

	.cfg-select:focus {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}

	/* Live-session footer: hint + the "switch" (reconfigure) action. */
	.cfg-foot {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding-top: var(--space-1);
		border-top: 1px solid var(--border-subtle);
	}

	.cfg-hint {
		flex: 1;
		font-size: 10.5px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		line-height: 1.3;
	}

	.cfg-apply {
		flex-shrink: 0;
		padding: 4px 10px;
	}

	.input-hint {
		font-size: 10.5px;
		color: var(--text-disabled);
		font-family: var(--font-mono);
		margin-top: 5px;
		padding-left: 2px;
	}

	@media (prefers-reduced-motion: reduce) {
		.item-text.streaming::after {
			animation: none;
		}
		.streaming-dot {
			animation: none;
		}
		.plan-spinner {
			animation: none;
		}
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
		.detail {
			height: auto;
			border-left: none;
			border-top: 1px solid var(--border-subtle);
		}
	}
</style>
