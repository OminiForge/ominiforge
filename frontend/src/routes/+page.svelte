<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { client } from '$lib/client';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import { statLabel, formatCost, cacheLabel, topTools } from '$lib/stats';
	import Button from '$lib/components/Button.svelte';

	/** One dashboard card: a session's metadata plus its folded summary. `summary`
	 *  is null when the per-session fold failed — the card still renders (title +
	 *  time), just without metrics, so one bad session never blanks the grid. */
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
					// Summary is best-effort: a fold failure must not drop the card.
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
		// Open a draft; the real session is created lazily on first send
		// (sessions/[id]), so clicking "New session" never leaves an empty one.
		void goto('/sessions/new');
	}

	// u64 fields arrive as JS number|bigint over JSON; coerce defensively.

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
			<h1>Dashboard</h1>
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
			<ul class="grid">
				{#each rows as row (row.meta.id)}
					{@const s = row.summary}
					{@const ws = workspace(row.meta)}
					{@const badge = originBadge(row.meta)}
					{@const tools = s ? topTools(s, 4) : []}
					<li>
						<a href={`/sessions/${row.meta.id}`} class="card">
							<div class="card-head">
								<div class="card-title" class:untitled={!s?.first_user_input}>
									{title(row)}
								</div>
								<div class="card-sub">
									{#if ws}<span class="chip ws">{ws}</span>{/if}
									<span class="time">{formatTime(row.meta.created_at)}</span>
									{#if badge}<span class="origin-badge">{badge}</span>{/if}
								</div>
							</div>

							{#if s}
								<div class="stats">
									<div class="stat">
										<span class="stat-value">{s.total_turns}</span>
										<span class="stat-label">{statLabel.turns(s.total_turns)}</span>
									</div>
									<div class="stat">
										<span class="stat-value">{s.total_model_requests}</span>
										<span class="stat-label">{statLabel.reqs(s.total_model_requests)}</span>
									</div>
									<div class="stat">
										<span class="stat-value">
											{s.total_tool_calls}{#if s.total_tool_failures > 0}<span class="stat-fail">/{s.total_tool_failures}✗</span>{/if}
										</span>
										<span class="stat-label">{statLabel.toolCalls(s.total_tool_calls)}</span>
									</div>
									<div class="stat">
										<span class="stat-value cost" class:unpriced={s.cost_usd == null}>{formatCost(s)}</span>
										<span class="stat-label">{statLabel.cost}</span>
									</div>
									<div class="stat">
										<span class="stat-value">{cacheLabel(s)}</span>
										<span class="stat-label">{statLabel.cache}</span>
									</div>
								</div>

								{#if tools.length > 0}
									<ul class="bars">
										{#each tools as t (t.tool)}
											<li class="bar-row">
												<span class="bar-label">{t.tool}</span>
												<span class="bar-track"><span class="bar-fill" style="width: {t.pct}%"></span></span>
												<span class="bar-count">{t.count}</span>
											</li>
										{/each}
									</ul>
								{/if}
							{/if}
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

	/* Full-bleed: dashboard fills the whole main area; no centered reading
	 * column. The grid wraps cards across the full width on wide displays. */
	.page-inner {
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
	/* STYLE-APPEND */

	.grid {
		list-style: none;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
		gap: var(--space-3);
	}

	.grid li {
		/* Grid items default to min-width:auto; a long nowrap title would push the
		 * item past its column and spawn a stray scrollbar. min-width:0 lets the
		 * title ellipsis engage instead. */
		min-width: 0;
	}

	.card {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		min-width: 0;
		height: 100%;
		padding: var(--space-4);
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
	/* CARD-APPEND */

	.card-head {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		min-width: 0;
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

	.card-sub {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}

	.chip.ws {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-secondary);
		padding: 1px 6px;
		border-radius: 3px;
		background: var(--canvas-float);
		border: 1px solid var(--border-subtle);
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
	/* STATS-APPEND */

	.stats {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-4);
		padding-top: var(--space-3);
		border-top: 1px solid var(--border-subtle);
		margin-top: auto;
	}

	.stat {
		display: flex;
		flex-direction: column;
		gap: 1px;
	}

	.stat-value {
		font-family: var(--font-mono);
		font-size: 14px;
		font-weight: 500;
		color: var(--text-primary);
		font-variant-numeric: tabular-nums;
		line-height: 1.2;
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

	.stat-label {
		font-size: 9.5px;
		font-weight: 510;
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--text-tertiary);
	}

	.bars {
		list-style: none;
		display: grid;
		gap: 5px;
	}

	.bar-row {
		display: grid;
		grid-template-columns: minmax(56px, 96px) 1fr auto;
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
		/* Muted, not accent: bars are dense data, not the screen's one CTA. */
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



</style>



