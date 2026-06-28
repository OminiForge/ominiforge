<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { client } from '$lib/client';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import Button from '$lib/components/Button.svelte';

	/** One list row: a session's metadata plus its derived summary. `summary` is
	 *  null when the per-session summary fetch failed — the row still renders,
	 *  just without a title/metrics, so one bad session never blanks the list. */
	interface Row {
		meta: SessionMeta;
		summary: SessionSummary | null;
	}

	let rows = $state<Row[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function refresh() {
		loading = true;
		error = null;
		try {
			const ids = await client.listSessions();
			rows = await Promise.all(
				ids.map(async (id): Promise<Row> => {
					const meta = await client.getSession(id);
					// Summary is best-effort: a fold failure must not drop the row.
					const summary = await client.getSummary(id).catch(() => null);
					return { meta, summary };
				})
			);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}

	function create() {
		// Don't create the session yet — open a draft conversation. The real
		// session is created lazily on the first send (see sessions/[id]), so
		// merely clicking "New session" never leaves an empty session behind.
		void goto('/sessions/new');
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

	/** Card title: the opening user message, else a workspace/id fallback so a
	 *  title-less session (draft never sent, or summary fetch failed) is still
	 *  distinguishable. */
	function title(row: Row): string {
		const first = row.summary?.first_user_input?.trim();
		if (first) return clip(first, 96);
		const ws = workspace(row.meta);
		return ws ?? shortId(row.meta.id);
	}

	function clip(s: string, n: number): string {
		const line = s.split('\n')[0];
		return line.length > n ? line.slice(0, n) + '…' : line;
	}

	function workspace(meta: SessionMeta): string | null {
		if (!meta.workspace) return null;
		return meta.workspace.split('/').filter(Boolean).pop() ?? meta.workspace;
	}

	function shortId(id: string): string {
		return id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
	}

	function originBadge(meta: SessionMeta): string | null {
		if (meta.origin.kind === 'fork') return 'fork';
		if (meta.origin.kind === 'compaction') return 'compacted';
		if (meta.origin.kind === 'reconfiguration') return 'reconfigured';
		return null;
	}

	onMount(refresh);
</script>

<div class="page">
	<div class="page-inner">
		<header>
			<h1>Sessions</h1>
			<Button variant="accent" onclick={create}>New session</Button>
		</header>

		{#if error}
			<p class="error">{error}</p>
		{/if}

		{#if loading}
			<p class="muted">加载中…</p>
		{:else if rows.length === 0}
			<p class="muted">还没有会话，创建一个开始吧。</p>
		{:else}
			<ul class="list">
				{#each rows as row (row.meta.id)}
					{@const s = row.summary}
					{@const ws = workspace(row.meta)}
					{@const badge = originBadge(row.meta)}
					<li>
						<a href={`/sessions/${row.meta.id}`} class="card">
							<div class="card-title" class:untitled={!s?.first_user_input}>
								{title(row)}
							</div>
							<div class="card-meta">
								{#if ws}<span class="meta-chip ws">{ws}</span>{/if}
								{#if s}
									<span class="meta-chip">{s.total_turns} turns</span>
									{#if s.total_tool_calls > 0}
										<span class="meta-chip">{s.total_tool_calls} tools</span>
									{/if}
									{#if s.cost_usd != null}
										<span class="meta-chip cost">${s.cost_usd.toFixed(s.cost_usd < 0.01 ? 4 : 2)}</span>
									{/if}
								{/if}
							</div>
							<div class="card-footer">
								<span class="time">{formatTime(row.meta.created_at)}</span>
								{#if badge}<span class="origin-badge">{badge}</span>{/if}
							</div>
						</a>
					</li>
				{/each}
			</ul>
		{/if}
	</div>
</div>

<style>
	.page {
		height: 100%;
		overflow-y: auto;
	}

	/* The scroll container (.page) spans the full main area so its vertical
	 * scrollbar sits at the viewport's right edge; the inner wrapper holds the
	 * 880px reading column. Putting max-width on .page instead would park the
	 * scrollbar at the 880px mark — mid-screen on wide displays. */
	.page-inner {
		max-width: 880px;
		padding: var(--space-8) var(--space-10);
	}

	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-bottom: var(--space-6);
		padding-bottom: var(--space-4);
		border-bottom: 1px solid var(--border-subtle);
	}

	h1 {
		font-size: 22px;
		font-weight: 600;
		letter-spacing: -0.01em;
	}

	.error {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		padding: var(--space-3) var(--space-4);
		border-radius: var(--radius-md);
		border: 1px solid color-mix(in srgb, var(--state-error) 25%, transparent);
		margin-bottom: var(--space-4);
		font-size: 13px;
	}

	.muted {
		color: var(--text-tertiary);
		font-size: 13px;
		text-align: center;
		padding: var(--space-12);
	}

	.list {
		list-style: none;
		display: grid;
		gap: var(--space-2);
	}

	.list li {
		/* Grid items default to min-width:auto, which lets a long, nowrap card
		 * title push the item past the column width — overflowing the 880px page
		 * and spawning a stray horizontal scrollbar. min-width:0 lets it shrink so
		 * the title's ellipsis engages instead. */
		min-width: 0;
	}

	.card {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		min-width: 0;
		padding: var(--space-3) var(--space-4);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
		transition:
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
	}

	.card:hover {
		background: var(--surface-hover);
		border-color: var(--border-default);
	}

	.card-title {
		color: var(--text-primary);
		font-weight: 500;
		font-size: 13.5px;
		line-height: 1.5;
		font-family: var(--font-chinese);
		min-width: 0;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.card-title.untitled {
		color: var(--text-tertiary);
		font-family: var(--font-mono);
		font-weight: 450;
	}

	.card-meta {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}

	.meta-chip {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
	}

	.meta-chip.ws {
		color: var(--text-secondary);
		padding: 1px 6px;
		border-radius: 3px;
		background: var(--canvas-float);
		border: 1px solid var(--border-subtle);
	}

	.meta-chip.cost {
		color: var(--accent-ink);
	}

	.card-footer {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.time {
		color: var(--text-tertiary);
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}

	.origin-badge {
		font-size: 10px;
		font-weight: 510;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		padding: 1px 5px;
		border-radius: 3px;
		color: var(--text-tertiary);
		background: var(--canvas-float);
		border: 1px solid var(--border-subtle);
	}
</style>
