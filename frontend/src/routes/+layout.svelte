<script lang="ts">
	import '$lib/styles/tokens.css';
	import '$lib/styles/global.css';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { currentSession, currentRuntime, currentRuntimeModels } from '$lib/stores/currentSession';

	let { children } = $props();

	const nav = [
		{ href: '/sessions', label: 'Sessions' },
		{ href: '/monitor', label: 'Monitor' },
		{ href: '/evolution', label: 'Evolution' }
	];

	/** Runtime-layer models that diverge from the configured model: models a
	 *  RequestStarted actually used that aren't the config-layer selection (a
	 *  subagent/fork on a different model). Empty until the config model is known,
	 *  so we never flag divergence we can't yet judge. Surfacing this is fail-loud
	 *  (CLAUDE.md #12); the displayed Model row stays the stable config layer. */
	const divergent = $derived(
		$currentRuntime
			? $currentRuntimeModels.filter((m) => m !== $currentRuntime!.model)
			: []
	);

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

	/** Short workspace label: last two path segments, full path on hover. */
	function wsLabel(ws: string): string {
		const parts = ws.split('/').filter(Boolean);
		return parts.length > 2 ? '…/' + parts.slice(-2).join('/') : ws;
	}
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

		{#if $currentSession}
			{@const s = $currentSession}
			<div class="sidebar-runtime">
				<div class="sidebar-label">Runtime</div>

				{#if s.workspace}
					<div class="rt-entry">
						<div class="rt-label">Workspace</div>
						<div class="rt-value" title={s.workspace}>{wsLabel(s.workspace)}</div>
					</div>
				{/if}

				{#if $currentRuntime && $currentRuntime.env.length > 0}
					<div class="rt-entry">
						<div class="rt-label">Env</div>
						<div class="rt-value" title={$currentRuntime.env.join(' · ')}>
							{$currentRuntime.env.join(' · ')}
						</div>
					</div>
				{/if}

				{#if $currentRuntime}
					<div class="rt-entry">
						<div class="rt-label">Model</div>
						<div class="rt-value" title={`${$currentRuntime.provider} · ${$currentRuntime.model}`}>
							{$currentRuntime.model}
						</div>
					</div>
				{/if}

				{#if divergent.length > 0}
					<div class="rt-entry rt-warn">
						<div class="rt-label rt-warn-label">⚠ Runtime</div>
						<div class="rt-value rt-warn-value" title={`runtime used ${divergent.join(', ')}, configured ${$currentRuntime?.model}`}>
							{divergent.join(' · ')} ≠ {$currentRuntime?.model}
						</div>
					</div>
				{/if}

				{#if s.profile_id}
					<div class="rt-entry">
						<div class="rt-label">Profile</div>
						<div class="rt-value">{s.profile_id}</div>
					</div>
				{/if}
			</div>
		{/if}

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
		display: flex;
		width: 100vw;
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

	/* RUNTIME — current session context, hidden when not on a session page */
	.sidebar-runtime {
		padding: var(--space-3) var(--space-3) var(--space-2);
		border-top: 1px solid var(--border-subtle);
	}

	.rt-entry {
		padding: 0 var(--space-1);
		margin-bottom: var(--space-3);
	}

	.rt-entry:last-child {
		margin-bottom: 0;
	}

	.rt-label {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
		margin-bottom: 2px;
		line-height: 1;
	}

	.rt-value {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-secondary);
		line-height: 1.4;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 100%;
	}

	/* Divergence marker: runtime model ≠ configured model (fail-loud, B4) */
	.rt-warn-label {
		color: var(--state-error-text);
	}

	.rt-warn-value {
		color: var(--state-error-text);
		white-space: normal;
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
			flex-direction: column;
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
		.sidebar-runtime {
			display: none;
		}
		.sidebar-bottom {
			border-top: none;
			margin-top: 0;
			padding: 0;
		}
	}
</style>
