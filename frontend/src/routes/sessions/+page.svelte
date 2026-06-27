<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { client } from '$lib/client';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import Button from '$lib/components/Button.svelte';

	let sessions = $state<SessionMeta[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function refresh() {
		loading = true;
		error = null;
		try {
			const ids = await client.listSessions();
			sessions = await Promise.all(ids.map(id => client.getSession(id)));
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
		const now = Date.now();
		const diff = now - date.getTime();
		const mins = Math.floor(diff / 60000);
		const hours = Math.floor(diff / 3600000);
		const days = Math.floor(diff / 86400000);
		if (mins < 1) return '刚刚';
		if (mins < 60) return `${mins}分钟前`;
		if (hours < 24) return `${hours}小时前`;
		if (days < 7) return `${days}天前`;
		return date.toLocaleDateString('zh-CN');
	}

	function label(meta: SessionMeta): string {
		const ws = meta.workspace ? meta.workspace.split('/').pop() || meta.workspace : null;
		const originBadge =
			meta.origin.kind === 'fork' ? ' [fork]' :
			meta.origin.kind === 'compaction' ? ' [compacted]' : '';
		const date = new Date(meta.created_at);
		const datePart = date.toLocaleDateString('zh-CN', { month: 'short', day: 'numeric' });
		const timePart = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' });
		const workspace = ws ? `${ws} · ` : '';
		return `${workspace}${datePart} ${timePart}${originBadge}`;
	}

	onMount(refresh);
</script>

<div class="page">
	<header>
		<h1>Sessions</h1>
		<Button variant="accent" onclick={create}>New session</Button>
	</header>

	{#if error}
		<p class="error">{error}</p>
	{/if}

	{#if loading}
		<p class="muted">加载中…</p>
	{:else if sessions.length === 0}
		<p class="muted">还没有会话，创建一个开始吧。</p>
	{:else}
		<ul class="list">
			{#each sessions as meta (meta.id)}
				<li>
					<a href={`/sessions/${meta.id}`}>
						<span class="label">{label(meta)}</span>
						<span class="time">{formatTime(meta.created_at)}</span>
					</a>
				</li>
			{/each}
		</ul>
	{/if}
</div>

<style>
	.page {
		height: 100%;
		overflow-y: auto;
		padding: var(--space-8) var(--space-10);
		max-width: 880px;
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

	.list li a {
		display: flex;
		justify-content: space-between;
		align-items: center;
		padding: var(--space-3) var(--space-4);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
		transition: all var(--dur-fast) var(--ease-out);
	}

	.label {
		color: var(--text-primary);
		font-weight: 450;
		font-size: 13px;
	}

	.time {
		color: var(--text-tertiary);
		font-size: 11.5px;
		font-variant-numeric: tabular-nums;
	}

	.list li a:hover {
		background: var(--surface-hover);
		border-color: var(--border-default);
	}
</style>
