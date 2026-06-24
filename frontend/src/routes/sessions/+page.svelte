<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { client } from '$lib/client';
	import Button from '$lib/components/Button.svelte';

	let sessions = $state<string[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let creating = $state(false);

	async function refresh() {
		loading = true;
		error = null;
		try {
			sessions = await client.listSessions();
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
	<p class="muted">Loading…</p>
{:else if sessions.length === 0}
	<p class="muted">No sessions yet. Create one to start.</p>
{:else}
	<ul class="list">
		{#each sessions as id (id)}
			<li>
				<a href={`/sessions/${id}`} class="mono">{id}</a>
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
	}

	h1 {
		font-size: 18px;
		font-weight: 600;
	}

	.error {
		color: var(--error);
		background: var(--error-bg);
		padding: var(--gap-sm) var(--gap-md);
		border-radius: var(--radius-md);
		margin-bottom: var(--gap-lg);
		font-size: 13px;
	}

	.muted {
		color: var(--text-muted);
	}

	.list {
		list-style: none;
		display: flex;
		flex-direction: column;
		gap: var(--gap-xs);
	}

	.list li a {
		display: block;
		padding: var(--gap-md);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
		background: var(--surface);
		color: var(--text-secondary);
	}

	.list li a:hover {
		background: var(--surface-hover);
		color: var(--text-primary);
		text-decoration: none;
	}

	.mono {
		font-family: var(--font-mono);
		font-size: 12px;
	}
</style>
