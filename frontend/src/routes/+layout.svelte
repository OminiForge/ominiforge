<script lang="ts">
	import '$lib/styles/tokens.css';
	import '$lib/styles/global.css';
	import { page } from '$app/state';

	let { children } = $props();

	const nav = [
		{ href: '/sessions', label: 'Sessions' },
		{ href: '/monitor', label: 'Monitor' },
		{ href: '/evolution', label: 'Evolution' }
	];

	function active(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(href + '/');
	}
</script>

<div class="shell">
	<nav class="sidebar">
		<div class="brand">ominiforge</div>
		<ul>
			{#each nav as item (item.href)}
				<li>
					<a href={item.href} class:active={active(item.href)}>{item.label}</a>
				</li>
			{/each}
		</ul>
	</nav>
	<main>
		{@render children()}
	</main>
</div>

<style>
	.shell {
		display: grid;
		grid-template-columns: 200px 1fr;
		height: 100vh;
	}

	.sidebar {
		background: var(--bg-secondary);
		border-right: 1px solid var(--border);
		padding: var(--gap-lg) var(--gap-md);
		display: flex;
		flex-direction: column;
		gap: var(--gap-xl);
	}

	.brand {
		font-family: var(--font-mono);
		font-weight: 600;
		color: var(--text-primary);
		padding: 0 var(--gap-sm);
	}

	.sidebar ul {
		list-style: none;
		display: flex;
		flex-direction: column;
		gap: var(--gap-xs);
	}

	.sidebar a {
		display: block;
		padding: var(--gap-sm) var(--gap-md);
		border-radius: var(--radius-md);
		color: var(--text-secondary);
		font-size: 13px;
	}

	.sidebar a:hover {
		background: var(--surface);
		color: var(--text-primary);
		text-decoration: none;
	}

	.sidebar a.active {
		background: var(--surface);
		color: var(--accent);
	}

	main {
		overflow-y: auto;
		padding: var(--gap-2xl);
	}
</style>
