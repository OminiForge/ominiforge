<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { client } from '$lib/client';
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import Button from '$lib/components/Button.svelte';

	let sessions = $state<SessionMeta[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let creating = $state(false);

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

	async function create() {
		creating = true;
		error = null;
		try {
			const id = await client.createSession();
			await goto(`/sessions/${id}`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			creating = false;
		}
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

<header>
	<h1>Sessions</h1>
	<Button variant="accent" disabled={creating} onclick={create}>
		{creating ? 'Creating…' : 'New session'}
	</Button>
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

<style>
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
		text-align: center;
		padding: var(--gap-2xl);
	}

	.list {
		list-style: none;
		display: grid;
		gap: var(--gap-md);
	}

	.list li a {
		display: flex;
		justify-content: space-between;
		align-items: center;
		padding: var(--gap-lg);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
		background: var(--surface);
		transition: all var(--motion-fast);
	}

	.label {
		color: var(--text-primary);
		font-weight: 500;
	}

	.time {
		color: var(--text-muted);
		font-size: 13px;
	}

	.list li a:hover {
		background: var(--surface-hover);
		border-color: var(--border-hover);
		color: var(--text-primary);
		transform: translateY(-1px);
		box-shadow: var(--shadow-sm);
	}
</style>
