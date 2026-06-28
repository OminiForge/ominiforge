<script lang="ts">
	import '$lib/styles/tokens.css';
	import '$lib/styles/global.css';
	import { page } from '$app/state';
	import { onMount } from 'svelte';

	let { children } = $props();

	const nav = [
		{ href: '/', label: 'Dashboard' },
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
	<aside class="sidebar">
		<div class="sidebar-brand">
			<div class="brand-mark">
				<svg viewBox="0 0 12 12" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
					<rect x="1" y="7" width="10" height="2" rx="0.5" />
					<path d="M3 7V3.5C3 2.7 3.7 2 5 2h2c1.3 0 2 .7 2 1.5V7" />
					<rect x="5" y="4" width="2" height="3" />
				</svg>
			</div>
			<span class="brand-name">ominiforge</span>
		</div>

		<nav class="sidebar-section">
			<div class="sidebar-label">Nav</div>
			{#each nav as item (item.href)}
				<a href={item.href} class="nav-item" class:active={active(item.href)}>
					<span class="nav-dot"></span>
					{item.label}
				</a>
			{/each}
		</nav>

		<div class="sidebar-spacer"></div>

		<div class="sidebar-bottom">
			<button class="theme-btn" onclick={toggleTheme} title="切换主题">
				{#if theme === 'dark'}
					<svg width="11" height="11" viewBox="0 0 11 11" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true">
						<circle cx="5.5" cy="5.5" r="2.2" />
						<line x1="5.5" y1="0.5" x2="5.5" y2="1.8" />
						<line x1="5.5" y1="9.2" x2="5.5" y2="10.5" />
						<line x1="0.5" y1="5.5" x2="1.8" y2="5.5" />
						<line x1="9.2" y1="5.5" x2="10.5" y2="5.5" />
						<line x1="2" y1="2" x2="2.9" y2="2.9" />
						<line x1="8.1" y1="8.1" x2="9" y2="9" />
						<line x1="2" y1="9" x2="2.9" y2="8.1" />
						<line x1="8.1" y1="2.9" x2="9" y2="2" />
					</svg>
					Light
				{:else}
					<svg width="11" height="11" viewBox="0 0 11 11" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true">
						<path d="M9.5 6.2A4 4 0 1 1 4.8 1.5 3.1 3.1 0 0 0 9.5 6.2z" />
					</svg>
					Dark
				{/if}
			</button>
		</div>
	</aside>

	<main class="main">
		{@render children()}
	</main>
</div>

<style>
	.shell {
		display: grid;
		grid-template-columns: var(--sidebar-width) 1fr;
		height: 100vh;
		overflow: hidden;
	}

	.sidebar {
		width: var(--sidebar-width);
		min-width: var(--sidebar-width);
		height: 100%;
		background: var(--canvas-raised);
		border-right: 1px solid var(--border-subtle);
		display: flex;
		flex-direction: column;
		padding: var(--space-4) 0;
	}

	.sidebar-brand {
		padding: var(--space-3) var(--space-4) var(--space-4);
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.brand-mark {
		width: 22px;
		height: 22px;
		background: var(--accent);
		border-radius: var(--radius-sm);
		display: flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
	}

	.brand-mark svg {
		width: 12px;
		height: 12px;
		fill: var(--accent-fg);
	}

	.brand-name {
		font-size: 13px;
		font-weight: 590;
		color: var(--text-primary);
		letter-spacing: -0.02em;
	}

	.sidebar-section {
		padding: var(--space-3) var(--space-3) var(--space-1);
	}

	.sidebar-label {
		font-size: 10.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		padding: 0 var(--space-1);
		margin-bottom: var(--space-2);
	}

	.nav-item {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 5px var(--space-2);
		border-radius: var(--radius-sm);
		color: var(--text-secondary);
		font-size: 12.5px;
		font-weight: 450;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
		margin-bottom: 1px;
		text-decoration: none;
	}

	.nav-item:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	.nav-item.active {
		color: var(--text-primary);
		background: var(--surface-hover);
		font-weight: 510;
	}

	.nav-dot {
		width: 5px;
		height: 5px;
		border-radius: 50%;
		background: var(--text-disabled);
		flex-shrink: 0;
		transition: background var(--dur-fast) var(--ease-out);
	}

	.nav-item.active .nav-dot {
		background: var(--accent);
	}

	.sidebar-spacer {
		flex: 1;
	}

	.sidebar-bottom {
		padding: var(--space-3) var(--space-4) 0;
		border-top: 1px solid var(--border-subtle);
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin-top: var(--space-3);
	}

	.theme-btn {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		padding: 4px var(--space-2);
		border-radius: var(--radius-sm);
		border: 1px solid var(--border-default);
		background: transparent;
		color: var(--text-tertiary);
		font-size: 11px;
		cursor: pointer;
		transition: all var(--dur-fast) var(--ease-out);
	}

	.theme-btn:hover {
		color: var(--text-secondary);
		border-color: var(--border-strong);
	}

	.main {
		flex: 1;
		display: flex;
		flex-direction: column;
		height: 100%;
		overflow: hidden;
		min-width: 0;
	}

	@media (max-width: 768px) {
		.shell {
			grid-template-columns: 1fr;
			grid-template-rows: auto 1fr;
		}
		.sidebar {
			width: 100%;
			min-width: 0;
			height: auto;
			flex-direction: row;
			align-items: center;
			padding: var(--space-2) var(--space-3);
			border-right: none;
			border-bottom: 1px solid var(--border-subtle);
		}
		.sidebar-brand {
			padding: 0 var(--space-3) 0 0;
		}
		.sidebar-section {
			display: flex;
			align-items: center;
			gap: var(--space-1);
			padding: 0;
			flex-direction: row;
		}
		.sidebar-section .sidebar-label {
			display: none;
		}
		.sidebar-bottom {
			border-top: none;
			margin-top: 0;
			padding: 0;
		}
	}
</style>
