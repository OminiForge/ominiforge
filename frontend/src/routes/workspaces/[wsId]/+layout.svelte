<script lang="ts">
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { client } from '$lib/client';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import SessionStatusIcon from '$lib/components/SessionStatusIcon.svelte';
	import { connectStatus, viewState } from '$lib/status.svelte';

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
	let loading = $state(true);
	let error = $state<string | null>(null);
	// The workspace's absolute path, taken from the first session's meta — only
	// used for the sidebar header label (a session's meta already carries it).
	let workspacePath = $state<string | null>(null);
	// Generation counter: bumped on every workspace switch so a slow summary
	// backfill from a previous workspace can't write into the current list.
	let gen = 0;

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
				{#each rows as row (row.meta.id)}
					{@const badge = originBadge(row.meta)}
					<a
						href={`/workspaces/${workspaceId}/sessions/${row.meta.id}`}
						class="session-row"
						class:active={row.meta.id === activeSessionId}
					>
						<div class="row-title" class:untitled={!row.summary?.first_user_input}>
							{title(row)}
						</div>
						<div class="row-sub">
							<SessionStatusIcon state={viewState(row.meta.id)} />
							<span class="row-time">{formatTime(row.meta.created_at)}</span>
							{#if badge}<span class="row-badge">{badge}</span>{/if}
						</div>
					</a>
				{/each}
			{/if}
		</div>
	</aside>

	<div class="ws-main">
		{@render children()}
	</div>
</div>

<style>
	.panel {
		display: grid;
		grid-template-columns: 232px 1fr;
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
		flex-direction: column;
		gap: 2px;
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-sm);
		text-decoration: none;
		border: 1px solid transparent;
		transition:
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
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
