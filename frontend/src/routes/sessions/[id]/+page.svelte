<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { browser } from '$app/environment';
	import { page } from '$app/state';
	import { goto, replaceState } from '$app/navigation';
	import { Marked } from 'marked';
	import hljs from 'highlight.js/lib/common';
	import DOMPurify from 'dompurify';
	import { client } from '$lib/client';
	import type { EventSubscription } from '$lib/client-core';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import type { ProfileSummary } from '$lib/types/ProfileSummary';
	import type { ModelSummary } from '$lib/types/ModelSummary';
	import { apply, emptyState, type ConversationState, type Item, type PlanStep } from '$lib/conversation';
	import { takeDraftConfig } from '$lib/draft-config';
	import { num, statLabel, formatCost, cacheLabel, topTools } from '$lib/stats';

	/** Sentinel id for a not-yet-created (draft) session. Reaching `/sessions/new`
	 *  shows an empty conversation; the real session is created lazily on the
	 *  first send, so merely opening a draft never litters the store with empty
	 *  sessions. The backend never mints `new` as a real id, so it can't clash. */
	const DRAFT_ID = 'new';

	/** When the user is within this many pixels from the bottom we consider
	 *  them "at the bottom" and auto-scroll on new content. This tolerance
	 *  avoids missing the trigger due to sub-pixel rounding or small
	 *  layout shifts. */
	const SCROLL_BOTTOM_THRESHOLD = 80;

	let convo = $state<ConversationState>(emptyState());
	let input = $state('');
	let sending = $state(false);
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
	let selWorkspace = $state('');
	let cfgOpen = $state(false);
	let sessionId = $state(page.params.id!);
	let meta = $state<SessionMeta | null>(null);
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
	// Track collapsed state for reasoning items separately (index → collapsed)
	let collapsed = $state<Record<number, boolean>>({});
	// Whether the right detail rail (INFO + STATS) is shown. Persisted so the
	// choice survives reloads/navigation; defaults open. On narrow screens the
	// user can collapse it to give the conversation the full width.
	let detailOpen = $state(true);
	// Whether we're in the initial event-replay phase (loading existing history
	// from the durable log). During replay, events arrive in rapid bursts and we
	// use instant scroll instead of `behavior: 'smooth'` to avoid the visually
	// uncomfortable rapid-scrolling animation through the entire conversation.
	let isReplaying = $state(false);
	let replayDebounce: ReturnType<typeof setTimeout> | undefined;

	function toggleDetail() {
		detailOpen = !detailOpen;
		localStorage.setItem('detailOpen', detailOpen ? '1' : '0');
	}

	onMount(() => {
		// Restore the persisted rail state; default open when unset.
		detailOpen = localStorage.getItem('detailOpen') !== '0';
	});

	const isDraft = $derived(sessionId === DRAFT_ID);

	/** Returns `true` when the element is scrolled close enough to the bottom. */
	function isNearBottom(el: HTMLElement): boolean {
		return el.scrollHeight - el.scrollTop - el.clientHeight < SCROLL_BOTTOM_THRESHOLD;
	}

	/** Called whenever the user scrolls inside the stream container.
	 *  Updates `shouldAutoScroll` so that we only auto-scroll when the
	 *  user hasn't deliberately scrolled up to read history. */
	function onStreamScroll() {
		if (streamEl) {
			shouldAutoScroll = isNearBottom(streamEl);
		}
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
		selWorkspace = meta?.workspace ?? '';
		await refreshSummary(id);
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
		convo = emptyState();
		collapsed = {};
		context = null;
		// A new subscription means fresh content – start auto-scrolling.
		shouldAutoScroll = true;
		// Enter replay mode: the gateway will replay committed events in rapid
		// succession. During replay we use instant scroll (no smooth animation)
		// and debounce — once events stop arriving we consider replay done.
		isReplaying = true;
		clearTimeout(replayDebounce);
		sub = client.subscribeEvents(id, {
			onEvent: (ev) => {
				convo = apply(convo, ev);
				if (ev.type === 'compacted') {
					sessionId = ev.new_session_id;
					subscribe(ev.new_session_id);
					void loadMeta(ev.new_session_id);
				}
				// A settled turn means the fold's aggregates changed — refresh the
				// STATS snapshot so turns/cost/tokens track the live conversation.
				if (ev.type === 'turn_settled') {
					void refreshSummary(id);
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
		});
	}

	$effect(() => {
		const id = sessionId;
		// Draft: show an empty conversation, don't subscribe or load meta. The
		// real session doesn't exist yet — it's created on the first send().
		if (id === DRAFT_ID) {
			sub?.close();
			convo = emptyState();
			collapsed = {};
			meta = null;
			runtime = null;
			summary = null;
			context = null;
			// Adopt a config stashed by the dashboard's "New session ▾" (read-once;
			// it clears itself), so the draft opens prefilled with that choice.
			const pre = takeDraftConfig();
			if (pre.profile) selProfile = pre.profile;
			if (pre.model) selModel = pre.model;
			if (pre.workspace) selWorkspace = pre.workspace;
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
		if (!text || sending) return;
		sending = true;
		error = null;
		try {
			if (sessionId === DRAFT_ID) {
				// Lazily create the real session on first send, then adopt its id.
				// Pass the draft picker's choices (only the set ones) so the session
				// is created on the chosen profile / model / workspace.
				// Setting sessionId drives the $effect to subscribe + load meta;
				// replaceState swaps the URL without pushing a history entry (so
				// Back doesn't return to the empty draft).
				const realId = await client.createSession({
					profile: selProfile || undefined,
					model: selModel || undefined,
					workspace: selWorkspace.trim() || undefined
				});
				sessionId = realId;
				replaceState(`/sessions/${realId}`, {});
				await client.sendMessage(realId, text);
			} else {
				await client.sendMessage(sessionId, text);
			}
			input = '';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			sending = false;
		}
	}

	async function cancel() {
		try {
			await client.cancel(sessionId);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
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
		// Auto-collapse finished reasoning and completed tools
		if (item.kind === 'reasoning') return !item.streaming;
		if (item.kind === 'tool') return item.status === 'done' || item.status === 'error';
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

	/** One-line summary of a tool call's arguments, shown when the tool is
	 *  collapsed so the user sees *what* ran, not just the tool name. Pulls the
	 *  most meaningful field (command/path/query/…) from the parsed JSON args,
	 *  falling back to the raw args string. */
	function toolPreview(args: string): string {
		if (!args || args === '{}') return '';
		let parsed: unknown;
		try {
			parsed = JSON.parse(args);
		} catch {
			return clip(args, 80);
		}
		if (parsed && typeof parsed === 'object') {
			const obj = parsed as Record<string, unknown>;
			const keys = ['command', 'cmd', 'script', 'path', 'file', 'file_path', 'query', 'url', 'pattern'];
			for (const k of keys) {
				if (typeof obj[k] === 'string' && obj[k]) return clip(obj[k] as string, 80);
			}
			// No known key: show first string value, else compact JSON.
			const firstStr = Object.values(obj).find((v) => typeof v === 'string' && v);
			if (typeof firstStr === 'string') return clip(firstStr, 80);
		}
		return clip(args, 80);
	}

	function clip(s: string, n: number): string {
		const line = s.split('\n')[0];
		return line.length > n ? line.slice(0, n) + '…' : line;
	}

	/** Pretty-print + syntax-highlight tool-call JSON args into safe HTML.
	 *  Falls back to escaped raw text when args aren't valid JSON. Highlight
	 *  classes mirror the design tokens (--syntax-key/str/num). */
	function renderArgs(args: string): string {
		let pretty = args;
		try {
			pretty = JSON.stringify(JSON.parse(args), null, 2);
		} catch {
			return escapeHtml(args);
		}
		const esc = escapeHtml(pretty);
		// Keys: "foo": → highlight the key; strings / numbers as values.
		return esc
			.replace(/&quot;([^&]*?)&quot;(\s*:)/g, '<span class="syn-key">&quot;$1&quot;</span>$2')
			.replace(/:\s*&quot;([^&]*?)&quot;/g, ': <span class="syn-str">&quot;$1&quot;</span>')
			.replace(/:\s*(-?\d+(?:\.\d+)?)/g, ': <span class="syn-num">$1</span>');
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
	const cfgDirty = $derived(
		!isDraft && (selProfile !== curProfile || selModel !== curModel)
	);

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
			sessionId = newId;
			replaceState(`/sessions/${newId}`, {});
			// The $effect re-subscribes + reloads meta for the new id, which
			// refreshes curProfile/curModel via loadMeta below.
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			reconfiguring = false;
		}
	}
</script>

<div class="session-grid" class:no-detail={isDraft || !detailOpen}>
<div class="conv-page">
	<!-- TOPBAR -->
	<div class="topbar">
		<a href="/" class="topbar-back" title="返回首页" aria-label="Back to home">
			<svg width="14" height="14" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
				<polyline points="8.5,2.5 4,7 8.5,11.5" />
			</svg>
		</a>
		<span class="topbar-title">{topbarTitle()}</span>
		<div class="topbar-sep"></div>
		<div class="topbar-meta">
			{#if isDraft}
				<span class="mono draft-hint">draft · 发送后创建</span>
			{:else}
				<span class="mono">{shortId(sessionId)}</span>
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
				<svg width="14" height="14" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">
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
	<div class="conv-scroll" bind:this={streamEl} onscroll={onStreamScroll}>
		<div class="conv-inner">
			{#each convo.items as item, i (i)}
				{#if item.kind === 'user'}
					<div class="item item-user">
						<div class="user-bubble">{item.text}</div>
					</div>

				{:else if item.kind === 'text'}
					{#if item.text.trim()}
						<div class="item item-text" class:streaming={item.streaming}>
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
						<div class="item item-reasoning" class:expanded={!isCollapsed(item, i)}>
							<button class="reasoning-toggle" onclick={() => toggleCollapse(item, i)} aria-expanded={!isCollapsed(item, i)}>
								<svg class="reasoning-toggle-icon" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
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
					<div class="item">
						<div
							class="tool-block"
							class:done={item.status === 'done'}
							class:running={item.status === 'running'}
							class:error={item.status === 'error'}
							class:expanded={!isCollapsed(item, i)}
						>
							<button class="tool-header" onclick={() => toggleCollapse(item, i)} aria-expanded={!isCollapsed(item, i)}>
								<span class="tool-pip"></span>
								{#if item.status === 'running'}
									<span class="tool-spinner"></span>
								{/if}
								<span class="tool-name">{item.name}</span>
								<span class="tool-status-badge">{item.status}</span>
								{#if isCollapsed(item, i) && toolPreview(item.args)}
									<span class="tool-preview">{toolPreview(item.args)}</span>
								{/if}
								<svg class="tool-chevron" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
									<polyline points="4,2 8,6 4,10" />
								</svg>
							</button>
							{#if !isCollapsed(item, i)}
								<div class="tool-detail">
									{#if item.args && item.args !== '{}'}
										<div class="tool-detail-section">
											<div class="tool-detail-label">params</div>
											<!-- eslint-disable-next-line svelte/no-at-html-tags -->
											<div class="tool-params">{@html renderArgs(item.args)}</div>
										</div>
									{/if}
									{#if item.result}
										<div class="tool-detail-section">
											<div class="tool-detail-label">result</div>
											<div class="tool-result">{item.result}</div>
										</div>
									{:else if item.status === 'running'}
										<div class="running-placeholder">
											<span class="tool-spinner"></span>
											正在执行…
										</div>
									{/if}
								</div>
							{/if}
						</div>
					</div>

				{:else if item.kind === 'plan'}
					<!-- Streaming placeholders (item.streaming) render nothing: a flashing
					     inline "planning…" card on every plan op is pure eye-strain, and
					     the dock already shows the live plan. The docked card is shown in
					     the dock, so skip it here too — only committed history cards render. -->
					{#if !item.streaming && i !== dockPlan?.index}
						{@const prog = planProgress(item.steps)}
						<div class="item">
							<div class="plan-card" class:expanded={!isCollapsed(item, i)} class:done={planDone(item.steps)}>
								<button class="plan-head" onclick={() => toggleCollapse(item, i)} aria-expanded={!isCollapsed(item, i)}>
									<svg class="plan-icon" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
										<path d="M3 3h8M3 7h8M3 11h5" />
									</svg>
									<span class="plan-title">Plan</span>
									<span class="plan-progress">{prog.done}/{prog.total}</span>
									<span class="plan-track"><span class="plan-bar" style="width: {prog.total ? (prog.done / prog.total) * 100 : 0}%"></span></span>
									<svg class="plan-chevron" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
										<polyline points="4,2 8,6 4,10" />
									</svg>
								</button>
								{#if !isCollapsed(item, i)}
									<ol class="plan-steps">
										{#each item.steps as step (step.id)}
											<li class="plan-step" data-status={step.status}>
												<span class="plan-step-mark" aria-hidden="true">
													{#if step.status === 'completed'}
														<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="2.5,6.5 5,9 9.5,3.5" /></svg>
													{:else if step.status === 'in_progress'}
														<span class="plan-spinner"></span>
													{:else if step.status === 'cancelled'}
														<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><line x1="3" y1="3" x2="9" y2="9" /><line x1="9" y1="3" x2="3" y2="9" /></svg>
													{:else if step.status === 'blocked'}
														<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="6" cy="6" r="4.2" /><line x1="3" y1="3" x2="9" y2="9" stroke-linecap="round" /></svg>
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
					<div class="item item-error">{item.message}</div>

				{:else if item.kind === 'notice'}
					<div class="item item-notice">{item.message}</div>
				{/if}
			{/each}

			{#if convo.items.length === 0}
				<p class="empty">{isDraft ? '输入消息，开始一段新对话' : '发送消息开始对话'}</p>
			{/if}
		</div>
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
					<button class="plan-head" onclick={() => (pinnedPlanCollapsed = !pinnedPlanCollapsed)} aria-expanded={!pinnedPlanCollapsed}>
						<svg class="plan-icon" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
							<path d="M3 3h8M3 7h8M3 11h5" />
						</svg>
						<span class="plan-title">Plan</span>
						<span class="plan-progress">{prog.done}/{prog.total}</span>
						<span class="plan-track"><span class="plan-bar" style="width: {prog.total ? (prog.done / prog.total) * 100 : 0}%"></span></span>
						<svg class="plan-chevron" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
							<polyline points="4,2 8,6 4,10" />
						</svg>
					</button>
					{#if !pinnedPlanCollapsed}
						<ol class="plan-steps">
							{#each dockPlan.steps as step (step.id)}
								<li class="plan-step" data-status={step.status}>
									<span class="plan-step-mark" aria-hidden="true">
										{#if step.status === 'completed'}
											<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="2.5,6.5 5,9 9.5,3.5" /></svg>
										{:else if step.status === 'in_progress'}
											<span class="plan-spinner"></span>
										{:else if step.status === 'cancelled'}
											<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><line x1="3" y1="3" x2="9" y2="9" /><line x1="9" y1="3" x2="3" y2="9" /></svg>
										{:else if step.status === 'blocked'}
											<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="6" cy="6" r="4.2" /><line x1="3" y1="3" x2="9" y2="9" stroke-linecap="round" /></svg>
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
			<div class="input-box">
				<textarea
					class="input-field"
					bind:value={input}
					onkeydown={onKeydown}
					placeholder="输入消息… Enter 发送，Shift+Enter 换行"
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
							<svg width="12" height="12" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">
								<circle cx="7" cy="7" r="2.2" />
								<path d="M7 1.2v1.6M7 11.2v1.6M1.2 7h1.6M11.2 7h1.6M2.9 2.9l1.1 1.1M10 10l1.1 1.1M11.1 2.9 10 4M4 10l-1.1 1.1" />
							</svg>
							<span class="cfg-label">{cfgLabel}</span>
						</button>
						{#if cfgOpen}
							<div class="cfg-popover">
								<label class="cfg-field">
									<span class="cfg-key">Profile</span>
									<select class="cfg-select" bind:value={selProfile}>
										<option value="">默认</option>
										{#each profiles as p (p.name)}
											<option value={p.name}>{p.name}{p.description ? ` — ${p.description}` : ''}</option>
										{/each}
									</select>
								</label>
								<label class="cfg-field">
									<span class="cfg-key">Model</span>
									<select class="cfg-select" bind:value={selModel}>
										<option value="">默认（按 profile）</option>
										{#each models as m (`${m.provider}/${m.model_id}`)}
											<option value={`${m.provider}/${m.model_id}`}>{m.model_id} · {m.provider}</option>
										{/each}
									</select>
								</label>
								<label class="cfg-field">
									<span class="cfg-key">Workspace</span>
									{#if isDraft}
										<input
											class="cfg-input"
											type="text"
											bind:value={selWorkspace}
											placeholder="默认工作区（绝对路径）"
											spellcheck="false"
										/>
									{:else}
										<!-- Workspace is a session property, not reconfigurable: read-only on a live session. -->
										<input class="cfg-input" type="text" value={selWorkspace} readonly title="工作区不可更改（会话属性）" />
									{/if}
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
							<svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round">
								<line x1="1" y1="1" x2="9" y2="9" />
								<line x1="9" y1="1" x2="1" y2="9" />
							</svg>
							Cancel
						</button>
					{/if}
					<button class="input-btn primary" disabled={sending} onclick={send}>
						{sending ? 'Sending…' : 'Send'}
						<kbd>↵</kbd>
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
					<div class="kv-val" title={`${runtime.provider} · ${runtime.model}`}>{runtime.model}</div>
				</div>
			{/if}
			{#if divergent.length > 0}
				<div class="kv warn">
					<div class="kv-key warn-key">⚠ Runtime</div>
					<div class="kv-val warn-val" title={`runtime used ${divergent.join(', ')}, configured ${runtime?.model}`}>
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
			{@const pct = context.window ? Math.min(100, (context.tokens / context.window) * 100) : null}
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
							{s.total_tool_calls}{#if s.total_tool_failures > 0}<span class="stat-fail">/{s.total_tool_failures}✗</span>{/if}
						</span>
						<span class="stat-key">{statLabel.toolCalls(s.total_tool_calls)}</span>
					</div>
					<div class="stat">
						<span class="stat-value cost" class:unpriced={s.cost_usd == null}>{formatCost(s)}</span>
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
								<span class="bar-track"><span class="bar-fill" style="width: {t.pct}%"></span></span>
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
		justify-content: flex-end;
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

	/* ---- TOOL (the 120% detail) ---- */
	.tool-block {
		border-radius: var(--radius-md);
		overflow: hidden;
		border: 1px solid var(--border-subtle);
		transition: border-color var(--dur-std) var(--ease-out);
	}

	.tool-block.done {
		border-color: color-mix(in srgb, var(--state-done) 22%, transparent);
	}

	.tool-block.running {
		border-color: color-mix(in srgb, var(--state-running) 35%, transparent);
		animation: tool-running-pulse 2s ease-in-out infinite;
	}

	@keyframes tool-running-pulse {
		0%,
		100% {
			border-color: color-mix(in srgb, var(--state-running) 30%, transparent);
			box-shadow: 0 0 0 0 transparent;
		}
		50% {
			border-color: color-mix(in srgb, var(--state-running) 55%, transparent);
			box-shadow: 0 0 0 3px color-mix(in srgb, var(--state-running) 6%, transparent);
		}
	}

	.tool-block.error {
		border-color: color-mix(in srgb, var(--state-error) 30%, transparent);
	}

	.tool-header {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 7px var(--space-3);
		background: var(--canvas-overlay);
		cursor: pointer;
		user-select: none;
		transition: background var(--dur-fast) var(--ease-out);
		width: 100%;
		text-align: left;
	}

	.tool-header:hover {
		background: var(--canvas-float);
	}

	.tool-pip {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		flex-shrink: 0;
		position: relative;
		background: var(--text-tertiary);
	}
	.done .tool-pip {
		background: var(--state-done);
	}
	.running .tool-pip {
		background: var(--state-running);
	}
	.error .tool-pip {
		background: var(--state-error);
	}

	.running .tool-pip::after {
		content: '';
		position: absolute;
		inset: -3px;
		border-radius: 50%;
		border: 1.5px solid var(--state-running);
		opacity: 0;
		animation: pip-ripple 1.8s ease-out infinite;
	}

	@keyframes pip-ripple {
		0% {
			opacity: 0.7;
			transform: scale(0.5);
		}
		100% {
			opacity: 0;
			transform: scale(2.2);
		}
	}

	.tool-spinner {
		width: 11px;
		height: 11px;
		border: 1.5px solid color-mix(in srgb, var(--state-running) 25%, transparent);
		border-top-color: var(--state-running);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
		flex-shrink: 0;
	}

	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	.tool-name {
		font-family: var(--font-mono);
		font-size: 12px;
		font-weight: 500;
		color: var(--text-primary);
		letter-spacing: -0.01em;
		flex-shrink: 0;
	}

	.tool-status-badge {
		font-size: 10px;
		font-weight: 510;
		padding: 1px 5px;
		border-radius: 3px;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		flex-shrink: 0;
	}
	.done .tool-status-badge {
		background: var(--state-done-bg);
		color: var(--state-done-text);
		border: 1px solid color-mix(in srgb, var(--state-done) 25%, transparent);
	}
	.running .tool-status-badge {
		background: var(--state-running-bg);
		color: var(--state-running-text);
		border: 1px solid color-mix(in srgb, var(--state-running) 25%, transparent);
	}
	.error .tool-status-badge {
		background: var(--state-error-bg);
		color: var(--state-error-text);
		border: 1px solid color-mix(in srgb, var(--state-error) 25%, transparent);
	}

	.tool-preview {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-tertiary);
		font-family: var(--font-mono);
		font-size: 11px;
	}

	.tool-chevron {
		margin-left: auto;
		width: 12px;
		height: 12px;
		color: var(--text-tertiary);
		transition: transform var(--dur-std) var(--ease-out);
		flex-shrink: 0;
	}

	.tool-block.expanded .tool-chevron {
		transform: rotate(90deg);
	}

	.tool-detail {
		background: var(--canvas-base);
		border-top: 1px solid var(--border-subtle);
	}

	.tool-detail-section {
		padding: var(--space-3) var(--space-4);
		border-bottom: 1px solid var(--border-subtle);
	}
	.tool-detail-section:last-child {
		border-bottom: none;
	}

	.tool-detail-label {
		font-size: 10px;
		font-weight: 510;
		color: var(--text-tertiary);
		text-transform: uppercase;
		letter-spacing: 0.1em;
		margin-bottom: var(--space-2);
	}

	.tool-params {
		font-family: var(--font-mono);
		font-size: 11.5px;
		color: var(--text-secondary);
		line-height: 1.6;
		white-space: pre;
		overflow-x: auto;
		max-height: 200px;
	}

	.tool-params :global(.syn-key) {
		color: var(--syntax-key);
	}
	.tool-params :global(.syn-str) {
		color: var(--syntax-str);
	}
	.tool-params :global(.syn-num) {
		color: var(--syntax-num);
	}

	.tool-result {
		font-family: var(--font-mono);
		font-size: 11.5px;
		color: var(--text-secondary);
		line-height: 1.6;
		white-space: pre-wrap;
		word-break: break-word;
		max-height: 220px;
		overflow-y: auto;
	}

	.running-placeholder {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		font-size: 12px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		padding: var(--space-3) var(--space-4);
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

	.cfg-select,
	.cfg-input {
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

	.cfg-select:focus,
	.cfg-input:focus {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}

	.cfg-input::placeholder {
		color: var(--text-disabled);
	}

	/* Read-only workspace on a live session: dimmed, no edit affordance. */
	.cfg-input[readonly] {
		color: var(--text-tertiary);
		background: var(--canvas-float);
		cursor: default;
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

	kbd {
		font-family: var(--font-mono);
		font-size: 10px;
		background: var(--canvas-float);
		border: 1px solid var(--border-strong);
		border-radius: 3px;
		padding: 1px 4px;
		color: var(--text-tertiary);
	}

	.input-hint {
		font-size: 10.5px;
		color: var(--text-disabled);
		font-family: var(--font-mono);
		margin-top: 5px;
		padding-left: 2px;
	}

	@media (prefers-reduced-motion: reduce) {
		.tool-block.running {
			animation: none;
		}
		.tool-spinner {
			animation: none;
		}
		.running .tool-pip::after {
			animation: none;
		}
		.item-text.streaming::after {
			animation: none;
		}
		.streaming-dot {
			animation: none;
		}
		.plan-spinner {
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
