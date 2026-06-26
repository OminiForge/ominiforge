<script lang="ts">
	import '$lib/styles/tokens.css';
	import '$lib/styles/global.css';
	import { page } from '$app/state';
	import { onMount } from 'svelte';

	let { children } = $props();

	const nav = [
		{ href: '/sessions', label: 'Sessions' },
		{ href: '/monitor', label: 'Monitor' },
		{ href: '/evolution', label: 'Evolution' }
	];

	let theme = $state<'light' | 'dark'>('dark');

	function active(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(href + '/');
	}

	function toggleTheme() {
		theme = theme === 'dark' ? 'light' : 'dark';
		localStorage.setItem('theme', theme);
		document.documentElement.setAttribute('data-theme', theme);
	}

	onMount(() => {
		const stored = localStorage.getItem('theme') as 'light' | 'dark' | null;
		const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
		theme = stored ?? (prefersDark ? 'dark' : 'light');
		document.documentElement.setAttribute('data-theme', theme);
	});
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
		<button class="theme-toggle" onclick={toggleTheme} title="切换主题">
			{#if theme === 'dark'}
				<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
					<circle cx="12" cy="12" r="4"/>
					<path d="M12 2v2m0 16v2M4.93 4.93l1.41 1.41m11.32 11.32l1.41 1.41M2 12h2m16 0h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/>
				</svg>
			{:else}
				<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
					<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>
				</svg>
			{/if}
		</button>
	</nav>
	<main>
		{@render children()}
	</main>
</div>

<style>
	.shell {
		display: grid;
		grid-template-columns: 220px 1fr;
		height: 100vh;
	}

	.sidebar {
		background: var(--bg-secondary);
		border-right: 1px solid var(--border);
		padding: var(--gap-xl) var(--gap-md);
		display: flex;
		flex-direction: column;
		gap: var(--gap-xl);
	}

	.brand {
		font-weight: 600;
		font-size: 20px;
		color: var(--text-primary);
		padding: var(--gap-sm) var(--gap-md);
		letter-spacing: -0.01em;
	}

	.sidebar ul {
		list-style: none;
		display: flex;
		flex-direction: column;
		gap: var(--gap-xs);
		flex: 1;
	}

	.sidebar a {
		display: block;
		padding: var(--gap-sm) var(--gap-md);
		color: var(--text-secondary);
		font-size: 14px;
		font-weight: 500;
		border-radius: var(--radius-md);
		transition: all var(--motion-fast);
	}

	.sidebar a:hover {
		background: var(--surface-hover);
		color: var(--text-primary);
	}

	.sidebar a.active {
		background: var(--surface);
		color: var(--accent);
		font-weight: 600;
	}

	.theme-toggle {
		margin-top: auto;
		padding: var(--gap-sm) var(--gap-md);
		color: var(--text-secondary);
		border-radius: var(--radius-md);
		transition: all var(--motion-fast);
		display: flex;
		align-items: center;
		justify-content: center;
	}

	.theme-toggle:hover {
		background: var(--surface-hover);
		color: var(--text-primary);
	}

	main {
		overflow-y: auto;
		padding: var(--gap-2xl);
	}

	@media (max-width: 768px) {
		.shell {
			grid-template-columns: 1fr;
			grid-template-rows: auto 1fr;
		}
		.sidebar {
			flex-direction: row;
			align-items: center;
			padding: var(--gap-md);
			border-right: none;
			border-bottom: 1px solid var(--border);
		}
		.sidebar ul {
			flex-direction: row;
			gap: var(--gap-sm);
		}
		main {
			padding: var(--gap-lg);
		}
	}
</style>
