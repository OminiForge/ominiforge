<script lang="ts">
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { client } from '$lib/client';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import SessionStatusIcon from '$lib/components/SessionStatusIcon.svelte';
	import ConfirmDialog from '$lib/components/ConfirmDialog.svelte';
	import { connectStatus, viewState, latestSeqOf } from '$lib/status.svelte';

	let { children } = $props();

	// One shared gateway-wide status subscription for the whole app (ref-counted);
	// drives every row's live status icon without per-session requests.
	onMount(() => connectStatus());

	/** One sidebar row: a session's metadata plus its folded summary (for the
	 *  title). `summary` is null until the per-session fold arrives (or if it
	 *  failed) — the row still renders (id + time), so the list never blocks on
	 *  summaries and one bad session never blanks it. */
	interface Row {
		meta: SessionMeta;
		summary: SessionSummary | null;
	}

	const workspaceId = $derived(page.params.wsId!);
	// The selected session id from the route (`/workspaces/[wsId]/sessions/[id]`),
	// or undefined on the draft route (`/workspaces/[wsId]`). Drives the active
	// row highlight.
	const activeSessionId = $derived(page.params.id);

	let rows = $state<Row[]>([]);

	/** Rows ordered most-recently-active first. Activity is the last real user
	 *  message (`summary.last_user_message_at`), falling back to `created_at` for
	 *  a session whose summary hasn't backfilled yet (or that has no user turn).
	 *  So a session messaged moments ago floats to the top even if it was created
	 *  long before its neighbours. Non-destructive (sorts a copy) — the backfill /
	 *  insert logic keeps mutating `rows`, and this view re-derives. Rows may
	 *  reshuffle slightly as summaries stream in; that self-corrects to the true
	 *  order once each summary lands. */
	const sortedRows = $derived([...rows].sort((a, b) => activityTime(b) - activityTime(a)));

	function activityTime(row: Row): number {
		return new Date(row.summary?.last_user_message_at ?? row.meta.created_at).getTime();
	}

	let loading = $state(true);
	let error = $state<string | null>(null);
	// The workspace's absolute path, taken from the first session's meta — only
	// used for the sidebar header label (a session's meta already carries it).
	let workspacePath = $state<string | null>(null);
	// Generation counter: bumped on every workspace switch so a slow summary
	// backfill from a previous workspace can't write into the current list.
	let gen = 0;
	// Last activity seq we've already fetched a summary for, per session. Lets the
	// live-refresh effect fire only on a real advance, not on every status re-emit.
	let refreshedAt = new Map<string, number>();

	async function refresh(wsId: string) {
		loading = true;
		error = null;
		const myGen = ++gen;
		try {
			// Render the list immediately from metadata (one request), so the
			// sidebar and the conversation next to it paint without waiting on any
			// summary. Summaries (each a potentially multi-MB log replay) are then
			// backfilled sequentially in the background — crucially NOT via
			// Promise.all, which would fire N concurrent requests and, under the
			// browser's ~6-connection cap, starve the conversation's own SSE +
			// meta fetches (the "blank conversation in busy workspaces" bug).
			const metas = await client.listWorkspaceSessions(wsId);
			if (myGen !== gen) return; // a newer workspace switch superseded us
			workspacePath = metas.find((m) => m.workspace)?.workspace ?? null;
			rows = metas.map((meta) => ({ meta, summary: null }));
			loading = false;
			void backfillSummaries(myGen);
		} catch (e) {
			if (myGen !== gen) return;
			error = e instanceof Error ? e.message : String(e);
			loading = false;
		}
	}

	/** Fill in each row's summary one at a time, in the background. Sequential
	 *  (not parallel) so the conversation's live stream isn't crowded out; aborts
	 *  if the workspace changed (`gen` moved) so it never writes stale titles. */
	async function backfillSummaries(myGen: number) {
		for (const row of rows) {
			if (myGen !== gen) return;
			const summary = await client.getSummary(row.meta.id).catch(() => null);
			if (myGen !== gen) return;
			if (summary) {
				// Record the seq this summary reflects so the live-refresh effect
				// doesn't immediately refetch it; it fires only on a later advance.
				refreshedAt.set(row.meta.id, latestSeqOf(row.meta.id));
				// Replace the row in place so the title reactively updates.
				rows = rows.map((r) => (r.meta.id === row.meta.id ? { ...r, summary } : r));
			}
		}
	}

	// Load the session list when the workspace changes — and ONLY then. Switching
	// between sessions in the same workspace must not refetch the list (that was
	// the source of the per-switch summary burst).
	$effect(() => {
		const wsId = workspaceId;
		void refresh(wsId);
	});

	// When a new session is created (draft → real id via the send() goto), the
	// route gains an id that isn't in the list yet. Insert just that row (newest,
	// so on top), then fetch ONLY its summary so the title shows the first message
	// instead of the bare id — one metadata + one summary request, no burst, and a
	// no-op for a plain session switch (the id is already present).
	$effect(() => {
		const id = activeSessionId;
		if (!id || rows.some((r) => r.meta.id === id)) return;
		const wsId = workspaceId;
		void (async () => {
			const metas = await client.listWorkspaceSessions(wsId).catch(() => null);
			if (!metas) return;
			const fresh = metas.find((m) => m.id === id);
			if (!fresh || rows.some((r) => r.meta.id === id)) return;
			rows = [{ meta: fresh, summary: null }, ...rows];
			// Backfill this one session's summary so its title resolves to the first
			// message. The first turn was just ENQUEUED (sendMessage returns 202,
			// async), so the user event may not be committed to the log yet — poll a
			// few times until `first_user_input` lands rather than showing the bare
			// id. Abort if the workspace changed underneath us (gen moved).
			const myGen = gen;
			for (let attempt = 0; attempt < 8; attempt++) {
				const summary = await client.getSummary(id).catch(() => null);
				if (myGen !== gen || !rows.some((r) => r.meta.id === id)) return;
				if (summary?.first_user_input?.trim()) {
					rows = rows.map((r) => (r.meta.id === id ? { ...r, summary } : r));
					return;
				}
				await new Promise((resolve) => setTimeout(resolve, 400));
			}
		})();
	});

	// Keep the list order live: when a session's committed seq advances on the
	// status stream (a new turn ran — i.e. a new user message landed), refetch
	// that one session's summary so its `last_user_message_at` sort key and title
	// update without a page reload. Reads each row's `latestSeqOf` reactively, so
	// this re-runs whenever any tracked session's status advances. Only rows that
	// ALREADY have a summary are refreshed — the initial fetch stays owned by the
	// sequential `backfillSummaries`, so this never fans out N concurrent requests
	// on load; it's purely the live-update path (one fetch per real advance).
	$effect(() => {
		const myGen = gen;
		for (const row of rows) {
			const id = row.meta.id;
			const seq = latestSeqOf(id); // reactive dependency
			if (!row.summary) continue; // let backfill own the first fetch
			if (seq <= (refreshedAt.get(id) ?? 0)) continue;
			refreshedAt.set(id, seq);
			void (async () => {
				const summary = await client.getSummary(id).catch(() => null);
				if (myGen !== gen || !summary) return;
				rows = rows.map((r) => (r.meta.id === id ? { ...r, summary } : r));
			})();
		}
	});

	/** Open a fresh draft in this workspace. The workspace is fixed by the route
	 *  (`workspaceId`); the draft creates the session by id, resolving the path
	 *  server-side — no path is stashed or sent. */
	function newSession() {
		void goto(`/workspaces/${workspaceId}`);
	}

	function title(row: Row): string {
		const first = row.summary?.first_user_input?.trim();
		if (first) return clip(first, 60);
		return shortId(row.meta.id);
	}

	function clip(s: string, n: number): string {
		const line = s.split('\n')[0];
		return line.length > n ? line.slice(0, n) + '…' : line;
	}

	function shortId(id: string): string {
		return id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
	}

	function formatTime(iso: string): string {
		const date = new Date(iso);
		const diff = Date.now() - date.getTime();
		const mins = Math.floor(diff / 60000);
		const hours = Math.floor(diff / 3600000);
		const days = Math.floor(diff / 86400000);
		if (mins < 1) return '刚刚';
		if (mins < 60) return `${mins}分钟前`;
		if (hours < 24) return `${hours}小时前`;
		if (days < 7) return `${days}天前`;
		return date.toLocaleDateString('zh-CN');
	}

	function originBadge(meta: SessionMeta): string | null {
		if (meta.origin.kind === 'fork') return 'fork';
		if (meta.origin.kind === 'compaction') return 'compacted';
		if (meta.origin.kind === 'reconfiguration') return 'reconfigured';
		return null;
	}

	function wsLabel(path: string): string {
		const parts = path.split('/').filter(Boolean);
		return parts[parts.length - 1] ?? path;
	}

	// --- Archive / delete -------------------------------------------------------

	// A transient message for a failed row action (e.g. archiving a session with a
	// running turn → 409). Surfaced inline rather than swallowed.
	let actionError = $state<string | null>(null);

	// A row-action error belongs to the context it failed in — clear it on
	// navigation rather than letting it linger over an unrelated list.
	$effect(() => {
		void activeSessionId;
		actionError = null;
	});

	// The archived section: workspace-scoped like the live list above it. Loading
	// is owned by the effect below (fires on expand and on workspace switch), so
	// a stale list from another workspace can never show.
	let archivedOpen = $state(false);
	let archivedLoaded = $state(false);
	let archivedRows = $state<Row[]>([]);
	let archivedLoading = $state(false);
	let archivedError = $state<string | null>(null);

	$effect(() => {
		const wsId = workspaceId;
		if (!archivedOpen) {
			// Closed (or workspace switched while closed): drop the cache so the
			// count badge can't show another workspace's number.
			archivedLoaded = false;
			return;
		}
		void loadArchived(wsId);
	});

	function toggleArchived() {
		archivedOpen = !archivedOpen;
	}

	async function loadArchived(wsId: string) {
		archivedLoading = true;
		archivedError = null;
		try {
			const metas = await client.listArchivedSessions(wsId);
			archivedRows = metas.map((meta) => ({ meta, summary: null }));
			archivedLoaded = true;
		} catch (err) {
			archivedError = err instanceof Error ? err.message : String(err);
		} finally {
			archivedLoading = false;
		}
		// Backfill titles (first user message) one at a time, like the live list;
		// abort if the workspace changed underfoot (`gen` moved with `refresh`).
		const myGen = gen;
		for (const row of archivedRows) {
			if (myGen !== gen) return;
			const summary = await client.getSummary(row.meta.id).catch(() => null);
			if (myGen !== gen) return;
			if (summary) {
				archivedRows = archivedRows.map((r) => (r.meta.id === row.meta.id ? { ...r, summary } : r));
			}
		}
	}

	/** Archive a live session: retire it from this list (one-way). On success drop
	 *  its row and refresh the archived section (if open) so the session shows up
	 *  there; if it was the active session, fall back to the workspace root. A
	 *  running turn returns 409 — surface it, don't silently no-op. */
	async function archive(id: string, e: MouseEvent) {
		// The row is an `<a>`; keep archiving from navigating into the session.
		e.preventDefault();
		e.stopPropagation();
		actionError = null;
		try {
			await client.archiveSession(id);
			rows = rows.filter((r) => r.meta.id !== id);
			if (archivedOpen) await loadArchived(workspaceId);
			if (id === activeSessionId) void goto(`/workspaces/${workspaceId}`);
		} catch (err) {
			actionError = err instanceof Error ? err.message : String(err);
		}
	}

	// The session pending permanent deletion (drives the confirm dialog), a guard
	// so a double-click can't fire two DELETEs, and the failure from the last
	// attempt (shown inside the still-open dialog — writing it to the archived
	// section would hide it behind the modal).
	let pendingDelete = $state<string | null>(null);
	let deleting = $state(false);
	let deleteError = $state<string | null>(null);

	function closeDeleteDialog() {
		pendingDelete = null;
		deleteError = null;
	}

	async function confirmDelete() {
		const id = pendingDelete;
		if (!id || deleting) return;
		deleting = true;
		deleteError = null;
		try {
			await client.deleteSession(id);
			archivedRows = archivedRows.filter((r) => r.meta.id !== id);
			pendingDelete = null;
		} catch (err) {
			deleteError = err instanceof Error ? err.message : String(err);
		} finally {
			deleting = false;
		}
	}
</script>

<div class="panel">
	<aside class="ws-sidebar">
		<div class="ws-head">
			<a href="/" class="ws-back" title="返回工作区列表" aria-label="Back to workspaces">
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
			<div class="ws-title" title={workspacePath ?? workspaceId}>
				{workspacePath ? wsLabel(workspacePath) : '工作区'}
			</div>
		</div>

		<button class="new-session" onclick={newSession}>
			<svg
				width="12"
				height="12"
				viewBox="0 0 12 12"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				stroke-linecap="round"
			>
				<line x1="6" y1="2" x2="6" y2="10" />
				<line x1="2" y1="6" x2="10" y2="6" />
			</svg>
			New session
		</button>

		<div class="session-list">
			{#if loading}
				<p class="list-muted">加载中…</p>
			{:else if error}
				<p class="list-error">{error}</p>
			{:else if rows.length === 0}
				<p class="list-muted">还没有会话</p>
			{:else}
				{#if actionError}
					<p class="list-error">{actionError}</p>
				{/if}
				{#each sortedRows as row (row.meta.id)}
					{@const badge = originBadge(row.meta)}
					{@const vs = viewState(row.meta.id)}
					<a
						href={`/workspaces/${workspaceId}/sessions/${row.meta.id}`}
						class="session-row"
						class:active={row.meta.id === activeSessionId}
						class:unseen={vs === 'unseen'}
					>
						<div class="row-main">
							<div class="row-title" class:untitled={!row.summary?.first_user_input}>
								{title(row)}
							</div>
							<div class="row-sub">
								<SessionStatusIcon state={vs} />
								<span class="row-time"
									>{formatTime(row.summary?.last_user_message_at ?? row.meta.created_at)}</span
								>
								{#if badge}<span class="row-badge">{badge}</span>{/if}
							</div>
						</div>
						<button
							class="row-action"
							title="归档会话"
							aria-label="归档会话"
							onclick={(e) => archive(row.meta.id, e)}
						>
							<svg
								width="15"
								height="15"
								viewBox="0 0 24 24"
								fill="none"
								stroke="currentColor"
								stroke-width="2"
								stroke-linecap="round"
								stroke-linejoin="round"
							>
								<rect x="2" y="4" width="20" height="5" rx="1" />
								<path d="M4 9v9a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1V9" />
								<path d="M10 13h4" />
							</svg>
						</button>
					</a>
				{/each}
			{/if}
		</div>

		<!-- The archived list is a separate region from the active session list,
		     not its last item: it lives outside the scroll container, pinned to
		     the sidebar bottom, so the toggle stays visible no matter how long
		     the active list grows. Collapsed by default; expanding gives it its
		     own bounded scroll area (the active list shrinks to make room). -->
		<div class="archived-section">
			<button class="archived-toggle" onclick={toggleArchived} aria-expanded={archivedOpen}>
				<svg
					class="chevron"
					class:open={archivedOpen}
					width="12"
					height="12"
					viewBox="0 0 24 24"
					fill="none"
					stroke="currentColor"
					stroke-width="2.5"
					stroke-linecap="round"
					stroke-linejoin="round"
				>
					<polyline points="9 18 15 12 9 6" />
				</svg>
				已归档{#if archivedLoaded && archivedRows.length > 0}<span class="archived-count"
						>{archivedRows.length}</span
					>{/if}
			</button>
			{#if archivedOpen}
				<div class="archived-list">
					{#if archivedLoading}
						<p class="list-muted">加载中…</p>
					{:else if archivedError}
						<p class="list-error">{archivedError}</p>
					{:else if archivedRows.length === 0}
						<p class="list-muted archived-empty">没有已归档的会话</p>
					{:else}
						{#each archivedRows as row (row.meta.id)}
							<!-- An archived session is read-only browsable: the row links
							     to its conversation view (history replays from the log);
							     the only action left on it is the permanent delete. -->
							<a
								href={`/workspaces/${workspaceId}/sessions/${row.meta.id}`}
								class="session-row archived-row"
								class:active={row.meta.id === activeSessionId}
							>
								<div class="row-main">
									<div class="row-title" class:untitled={!row.summary?.first_user_input}>
										{title(row)}
									</div>
									<div class="row-sub">
										<span class="row-time"
											>{formatTime(row.summary?.last_user_message_at ?? row.meta.created_at)}</span
										>
									</div>
								</div>
								<button
									class="row-action danger"
									title="永久删除"
									aria-label="永久删除"
									onclick={(e) => {
										// The row is an `<a>`; keep deleting from navigating.
										e.preventDefault();
										e.stopPropagation();
										pendingDelete = row.meta.id;
										deleteError = null;
									}}
								>
									<svg
										width="15"
										height="15"
										viewBox="0 0 24 24"
										fill="none"
										stroke="currentColor"
										stroke-width="2"
										stroke-linecap="round"
										stroke-linejoin="round"
									>
										<polyline points="3 6 5 6 21 6" />
										<path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
										<path d="M10 11v6M14 11v6" />
										<path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2" />
									</svg>
								</button>
							</a>
						{/each}
					{/if}
				</div>
			{/if}
		</div>
	</aside>

	<div class="ws-main">
		{@render children()}
	</div>
</div>

{#if pendingDelete}
	<ConfirmDialog
		title="永久删除此会话？"
		confirmLabel={deleting ? '删除中…' : '永久删除'}
		error={deleteError}
		danger
		onconfirm={confirmDelete}
		oncancel={closeDeleteDialog}
	>
		此操作不可恢复：会话的全部文件（记录、事件、快照）将被彻底删除。
	</ConfirmDialog>
{/if}

<style>
	.panel {
		display: grid;
		grid-template-columns: var(--sidebar-width) 1fr;
		height: 100%;
		overflow: hidden;
		min-width: 0;
		/* The root `.main` is a flex column; as its flex item, `.panel` needs
		 * min-height:0 so its own `height:100%` + `overflow:hidden` actually
		 * clamp it. Without this, a long session list (flex-item default
		 * min-height:auto) grows the panel past the viewport and pushes the
		 * conversation's input box off-screen. */
		min-height: 0;
	}

	.ws-sidebar {
		height: 100%;
		border-right: 1px solid var(--border-subtle);
		background: var(--canvas-raised);
		display: flex;
		flex-direction: column;
		min-width: 0;
		/* Grid item in `.panel`; min-height:0 lets the inner `.session-list`
		 * own the scroll instead of the row stack growing this column. */
		min-height: 0;
	}

	.ws-head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		height: 44px;
		min-height: 44px;
		padding: 0 var(--space-3);
		border-bottom: 1px solid var(--border-subtle);
	}

	.ws-back {
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

	.ws-back:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	.ws-title {
		font-size: 13px;
		font-weight: 590;
		color: var(--text-primary);
		letter-spacing: -0.01em;
		font-family: var(--font-chinese);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.new-session {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: var(--space-2);
		margin: var(--space-3);
		padding: var(--gap-sm) var(--space-3);
		background: var(--accent);
		color: var(--accent-fg);
		border: 1px solid var(--accent);
		border-radius: var(--radius-md);
		font-size: 12.5px;
		font-weight: 590;
		cursor: pointer;
		transition: background var(--motion-fast);
	}

	.new-session:hover {
		background: var(--accent-hover);
		border-color: var(--accent-hover);
	}

	.session-list {
		flex: 1;
		overflow-y: auto;
		padding: 0 var(--space-2) var(--space-3);
		display: flex;
		flex-direction: column;
		gap: 2px;
		/* Flex child of the flex-column sidebar; min-height:0 is what makes
		 * overflow-y:auto engage instead of the list growing the sidebar. */
		min-height: 0;
	}

	.list-muted,
	.list-error {
		font-size: 12px;
		text-align: center;
		padding: var(--space-6) var(--space-2);
		font-family: var(--font-chinese);
	}
	.list-muted {
		color: var(--text-tertiary);
	}
	.list-error {
		color: var(--state-error-text);
	}

	.session-row {
		display: flex;
		flex-direction: row;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-sm);
		text-decoration: none;
		border: 1px solid transparent;
		transition:
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
	}

	/* The title/status stack; owns the row's flexible width so a long title
	 * ellipsizes instead of shoving the action button off the row. */
	.row-main {
		display: flex;
		flex-direction: column;
		gap: 2px;
		flex: 1;
		min-width: 0;
	}

	/* Row action (archive / delete): hidden until the row is hovered or the
	 * button itself is focused, matching the icon-button idiom used elsewhere. */
	.row-action {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		flex-shrink: 0;
		border-radius: var(--radius-sm);
		color: var(--text-tertiary);
		opacity: 0;
		transition:
			opacity var(--dur-fast) var(--ease-out),
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.session-row:hover .row-action,
	.row-action:focus-visible {
		opacity: 1;
	}

	.row-action:hover {
		color: var(--text-primary);
		background: var(--surface);
	}

	.row-action.danger:hover {
		color: var(--error-fg);
		background: var(--error);
	}

	.session-row:hover {
		background: var(--surface-hover);
	}

	.session-row.active {
		background: var(--surface-hover);
		border-color: var(--border-default);
	}

	.row-title {
		font-size: 12.5px;
		font-weight: 450;
		color: var(--text-primary);
		font-family: var(--font-chinese);
		line-height: 1.4;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		min-width: 0;
	}

	.session-row.active .row-title {
		font-weight: 510;
	}

	/* unseen: heavier title weight so "unread" rows pop at a glance.
	   Redundant with the status-icon dot (DESIGN.md §1.3: color+shape+motion). */
	.session-row.unseen .row-title {
		font-weight: 500;
	}
	.session-row.active.unseen .row-title {
		font-weight: 590;
	}

	.row-title.untitled {
		color: var(--text-tertiary);
		font-family: var(--font-mono);
	}

	.row-sub {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.row-time {
		font-size: 10.5px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
	}

	.row-badge {
		font-size: 9px;
		font-weight: 510;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		padding: 1px 4px;
		border-radius: 3px;
		color: var(--text-tertiary);
		background: var(--canvas-float);
		border: 1px solid var(--border-subtle);
	}

	/* Archived list: a separate region pinned to the sidebar bottom, not part of
	 * the active list's scroll container. flex-shrink:0 keeps the toggle visible
	 * no matter how long the active list grows; when expanded, its own list is
	 * height-capped and scrolls independently (the active list shrinks to make
	 * room). */
	.archived-section {
		flex-shrink: 0;
		border-top: 1px solid var(--border-subtle);
		padding: var(--space-2) var(--space-2) var(--space-2);
		display: flex;
		flex-direction: column;
		min-height: 0;
		max-height: 45%;
	}

	.archived-list {
		display: flex;
		flex-direction: column;
		gap: 2px;
		overflow-y: auto;
		min-height: 0;
	}

	.archived-toggle {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-sm);
		font-size: 11.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		cursor: pointer;
		transition: color var(--dur-fast) var(--ease-out);
	}

	.archived-toggle:hover {
		color: var(--text-secondary);
	}

	.chevron {
		flex-shrink: 0;
		transition: transform var(--dur-fast) var(--ease-out);
	}

	.chevron.open {
		transform: rotate(90deg);
	}

	.archived-count {
		font-size: 10px;
		font-variant-numeric: tabular-nums;
		padding: 0 5px;
		border-radius: var(--radius-lg);
		color: var(--text-tertiary);
		background: var(--canvas-float);
		border: 1px solid var(--border-subtle);
	}

	.archived-empty {
		padding: var(--space-3) var(--space-2);
	}

	/* Archived rows dim one notch; an untitled one (no first message yet) falls
	 * back to the shared `.untitled` mono-id styling. */
	.archived-row .row-title {
		color: var(--text-secondary);
	}

	.ws-main {
		min-width: 0;
		height: 100%;
		overflow: hidden;
		/* Grid item in `.panel`; min-height:0 so the conversation inside owns its
		 * own scroll and keeps its input box pinned in view. */
		min-height: 0;
	}

	@media (max-width: 768px) {
		.panel {
			grid-template-columns: 1fr;
			grid-template-rows: auto 1fr;
		}
		.ws-sidebar {
			height: auto;
			max-height: 40vh;
			border-right: none;
			border-bottom: 1px solid var(--border-subtle);
		}
	}
</style>
