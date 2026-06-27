<script lang="ts">
	import { onMount } from 'svelte';
	import { client } from '$lib/client';
	import type { SessionSummary } from '$lib/types/SessionSummary';

	let sessionIds = $state<string[]>([]);
	let selected = $state<string | null>(null);
	let summary = $state<SessionSummary | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);

	async function loadSessions() {
		error = null;
		try {
			sessionIds = await client.listSessions();
			if (sessionIds.length > 0) {
				selected = sessionIds[0];
				await loadSummary(selected);
			}
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	async function loadSummary(id: string) {
		loading = true;
		error = null;
		summary = null;
		try {
			summary = await client.getSummary(id);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}

	function onSelect(e: Event) {
		const id = (e.target as HTMLSelectElement).value;
		selected = id;
		void loadSummary(id);
	}

	// u64 fields arrive as JS numbers over JSON; coerce defensively.
	const n = (v: number | bigint): number => Number(v);

	const cost = $derived(summary?.cost_usd != null ? `$${summary.cost_usd.toFixed(4)}` : 'unpriced');
	const cacheRate = $derived(summary ? `${(summary.cache_hit_rate * 100).toFixed(1)}%` : '—');

	// tools_used map → sorted [{ tool, count }] for the bar chart.
	const toolData = $derived(
		summary
			? Object.entries(summary.tools_used)
					.map(([tool, count]) => ({ tool, count: n(count) }))
					.sort((a, b) => b.count - a.count)
			: []
	);

	const maxToolCount = $derived(Math.max(1, ...toolData.map((t) => t.count)));

	const errorData = $derived(
		summary
			? Object.entries(summary.errors)
					.map(([code, count]) => ({ code, count: n(count) }))
					.sort((a, b) => b.count - a.count)
			: []
	);

	function shortId(id: string): string {
		return id.length > 12 ? `${id.slice(0, 6)}…${id.slice(-4)}` : id;
	}

	onMount(loadSessions);
</script>

<div class="page">
<header>
	<h1>Monitor</h1>
	{#if sessionIds.length > 0}
		<select value={selected} onchange={onSelect}>
			{#each sessionIds as id (id)}
				<option value={id}>{shortId(id)}</option>
			{/each}
		</select>
	{/if}
</header>

{#if error}
	<p class="error">{error}</p>
{/if}

{#if loading}
	<p class="muted">加载中…</p>
{:else if sessionIds.length === 0}
	<p class="muted">还没有会话可供监控。</p>
{:else if summary}
	<div class="grid">
		<div class="stat">
			<span class="stat-label">Turns</span>
			<span class="stat-value">{summary.total_turns}</span>
		</div>
		<div class="stat">
			<span class="stat-label">Model requests</span>
			<span class="stat-value">{summary.total_model_requests}</span>
		</div>
		<div class="stat">
			<span class="stat-label">Tool calls</span>
			<span class="stat-value">{summary.total_tool_calls}</span>
			<span class="stat-sub">{summary.total_tool_failures} failed</span>
		</div>
		<div class="stat">
			<span class="stat-label">Cost</span>
			<span class="stat-value">{cost}</span>
		</div>
		<div class="stat">
			<span class="stat-label">Input tokens</span>
			<span class="stat-value">{n(summary.total_input_tokens).toLocaleString()}</span>
		</div>
		<div class="stat">
			<span class="stat-label">Output tokens</span>
			<span class="stat-value">{n(summary.total_output_tokens).toLocaleString()}</span>
		</div>
		<div class="stat">
			<span class="stat-label">Cache hit rate</span>
			<span class="stat-value">{cacheRate}</span>
			<span class="stat-sub">{n(summary.total_cache_read_tokens).toLocaleString()} read</span>
		</div>
	</div>

	<section class="panel">
		<h2>Tool usage</h2>
		{#if toolData.length > 0}
			<ul class="bars">
				{#each toolData as t (t.tool)}
					<li>
						<span class="bar-label">{t.tool}</span>
						<span class="bar-track">
							<span class="bar-fill" style="width: {(t.count / maxToolCount) * 100}%"></span>
						</span>
						<span class="bar-count">{t.count}</span>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="muted">无工具调用。</p>
		{/if}
	</section>

	{#if errorData.length > 0}
		<section class="panel">
			<h2>Errors</h2>
			<ul class="errlist">
				{#each errorData as e (e.code)}
					<li>
						<code>{e.code}</code>
						<span>{e.count}</span>
					</li>
				{/each}
			</ul>
		</section>
	{/if}
{/if}
</div>

<style>
	.page {
		height: 100%;
		overflow-y: auto;
		padding: var(--space-8) var(--space-10);
	}

	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-bottom: var(--gap-xl);
		padding-bottom: var(--gap-lg);
		border-bottom: 1px solid var(--border);
	}

	h1 {
		font-size: 24px;
		font-weight: 600;
		letter-spacing: -0.01em;
	}

	select {
		background: var(--surface);
		color: var(--text-primary);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
		padding: var(--gap-sm) var(--gap-md);
		font-family: var(--font-mono);
		font-size: 13px;
	}

	.error {
		color: var(--error);
		background: var(--error-bg);
		padding: var(--gap-md) var(--gap-lg);
		border-radius: var(--radius-md);
		border-left: 3px solid var(--error);
		margin-bottom: var(--gap-lg);
		font-size: 14px;
	}

	.muted {
		color: var(--text-muted);
		font-size: 14px;
		padding: var(--gap-lg);
	}

	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
		gap: var(--gap-md);
		margin-bottom: var(--gap-xl);
	}

	.stat {
		display: flex;
		flex-direction: column;
		gap: var(--gap-xs);
		padding: var(--gap-lg);
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
	}

	.stat-label {
		color: var(--text-muted);
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.stat-value {
		color: var(--text-primary);
		font-size: 24px;
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}

	.stat-sub {
		color: var(--text-secondary);
		font-size: 12px;
	}

	.panel {
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
		padding: var(--gap-lg);
		margin-bottom: var(--gap-lg);
	}

	.panel h2 {
		font-size: 14px;
		font-weight: 600;
		color: var(--text-secondary);
		margin-bottom: var(--gap-md);
	}

	.bars {
		list-style: none;
		display: grid;
		gap: var(--gap-sm);
	}

	.bars li {
		display: grid;
		grid-template-columns: minmax(80px, 160px) 1fr auto;
		align-items: center;
		gap: var(--gap-md);
		font-size: 13px;
	}

	.bar-label {
		color: var(--text-secondary);
		font-family: var(--font-mono);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.bar-track {
		background: var(--surface-hover);
		border-radius: var(--radius-sm);
		height: 16px;
		overflow: hidden;
	}

	.bar-fill {
		display: block;
		height: 100%;
		background: var(--accent);
		border-radius: var(--radius-sm);
		transition: width var(--motion-base);
	}

	.bar-count {
		color: var(--text-primary);
		font-variant-numeric: tabular-nums;
		min-width: 2ch;
		text-align: right;
	}

	.errlist {
		list-style: none;
		display: grid;
		gap: var(--gap-sm);
	}

	.errlist li {
		display: flex;
		justify-content: space-between;
		font-size: 13px;
		padding: var(--gap-xs) 0;
	}

	.errlist code {
		font-family: var(--font-mono);
		color: var(--error);
	}

	.errlist span {
		color: var(--text-secondary);
		font-variant-numeric: tabular-nums;
	}
</style>
