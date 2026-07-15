<script lang="ts">
	import '$lib/styles/tokens.css';
	import '$lib/styles/global.css';
	import { page } from '$app/state';
	import { onMount } from 'svelte';

	let { children } = $props();

	const nav = [
		{ href: '/', label: 'Dashboard', icon: 'dashboard' },
		{ href: '/evolution', label: 'Evolution', icon: 'evolution' }
	];

	let theme = $state<'light' | 'dark'>('dark');
	// Collapsed sidebar: shrinks to an icon rail (labels hidden, hover tooltips),
	// so the conversation gets the horizontal space back. Persisted like the
	// detail rail, defaults expanded so first-run users see the labels.
	let collapsed = $state(false);

	function active(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(href + '/');
	}

	function toggleTheme() {
		theme = theme === 'dark' ? 'light' : 'dark';
		localStorage.setItem('theme', theme);
		document.documentElement.setAttribute('data-theme', theme);
	}

	function toggleCollapsed() {
		collapsed = !collapsed;
		localStorage.setItem('navCollapsed', collapsed ? '1' : '0');
	}

	onMount(() => {
		const stored = localStorage.getItem('theme') as 'light' | 'dark' | null;
		const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
		theme = stored ?? (prefersDark ? 'dark' : 'light');
		document.documentElement.setAttribute('data-theme', theme);
		collapsed = localStorage.getItem('navCollapsed') === '1';
	});
</script>

<div class="shell" class:collapsed>
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
			<button
				class="collapse-btn"
				onclick={toggleCollapsed}
				title={collapsed ? '展开侧栏' : '收起侧栏'}
				aria-label="Toggle sidebar"
				aria-pressed={collapsed}
			>
				<!-- Original monoline "panel toggle": a framed panel with a movable
				     inner edge — the chevron flips with the collapsed state. -->
				<svg
					width="14"
					height="14"
					viewBox="0 0 14 14"
					fill="none"
					stroke="currentColor"
					stroke-width="1.4"
					stroke-linecap="round"
					stroke-linejoin="round"
					aria-hidden="true"
				>
					<rect x="1.5" y="2.5" width="11" height="9" rx="1.5" />
					<line x1="5.5" y1="2.5" x2="5.5" y2="11.5" />
				</svg>
			</button>
		</div>

		<nav class="sidebar-section">
			{#each nav as item (item.href)}
				<a
					href={item.href}
					class="nav-item"
					class:active={active(item.href)}
					title={collapsed ? item.label : undefined}
				>
					<span class="nav-icon" aria-hidden="true">
						{#if item.icon === 'dashboard'}
							<!-- Original monoline "dashboard": an asymmetric bento of four
							     panes — one tall, three stacked — read as a data console. -->
							<svg
								width="16"
								height="16"
								viewBox="0 0 16 16"
								fill="none"
								stroke="currentColor"
								stroke-width="1.4"
								stroke-linecap="round"
								stroke-linejoin="round"
							>
								<rect x="2" y="2" width="5" height="12" rx="1" />
								<rect x="9" y="2" width="5" height="5" rx="1" />
								<rect x="9" y="9" width="5" height="5" rx="1" />
							</svg>
						{:else if item.icon === 'evolution'}
							<!-- Original monoline "evolution": a branch splitting upward from a
							     root node into two forks — self-evolution / lineage. -->
							<svg
								width="16"
								height="16"
								viewBox="0 0 16 16"
								fill="none"
								stroke="currentColor"
								stroke-width="1.4"
								stroke-linecap="round"
								stroke-linejoin="round"
							>
								<circle cx="4" cy="12.5" r="1.6" />
								<circle cx="12" cy="3.5" r="1.6" />
								<circle cx="5.5" cy="3.5" r="1.6" />
								<path d="M4 10.9V7.5C4 5.8 4.7 4.7 5.5 5.1" />
								<path d="M4 8.2C4 6.5 8 6.5 10.6 4.3" />
							</svg>
						{/if}
					</span>
					<span class="nav-label">{item.label}</span>
				</a>
			{/each}
		</nav>

		<div class="sidebar-spacer"></div>

		<div class="sidebar-bottom">
			<a
				href="/settings"
				class="nav-item"
				class:active={active('/settings')}
				title={collapsed ? 'Settings' : '设置'}
				aria-label="Settings"
			>
				<span class="nav-icon" aria-hidden="true">
					<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
						<circle cx="12" cy="12" r="3" />
						<path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
					</svg>
				</span>
				<span class="nav-label">Settings</span>
			</a>
			<button
				class="nav-item theme-btn"
				onclick={toggleTheme}
				title={collapsed ? (theme === 'dark' ? 'Light' : 'Dark') : '切换主题'}
			>
				<span class="nav-icon" aria-hidden="true">
					{#if theme === 'dark'}
						<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
							<circle cx="12" cy="12" r="4.5" />
							<line x1="12" y1="2.5" x2="12" y2="5" />
							<line x1="12" y1="19" x2="12" y2="21.5" />
							<line x1="2.5" y1="12" x2="5" y2="12" />
							<line x1="19" y1="12" x2="21.5" y2="12" />
							<line x1="5.2" y1="5.2" x2="7" y2="7" />
							<line x1="17" y1="17" x2="18.8" y2="18.8" />
							<line x1="5.2" y1="18.8" x2="7" y2="17" />
							<line x1="17" y1="7" x2="18.8" y2="5.2" />
						</svg>
					{:else}
						<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
							<path d="M20 14.5A8 8 0 1 1 9.5 4 6.5 6.5 0 0 0 20 14.5z" />
						</svg>
					{/if}
				</span>
				<span class="nav-label">{theme === 'dark' ? 'Light' : 'Dark'}</span>
			</button>
		</div>
	</aside>

	<main class="main">
		{@render children()}
	</main>
</div>

<style>
	.shell {
		/* Local width so the collapsed rail is one source of truth for both the
		   grid column and the sidebar box. */
		--nav-width: var(--sidebar-width);
		display: grid;
		grid-template-columns: var(--nav-width) 1fr;
		height: 100vh;
		overflow: hidden;
		transition: grid-template-columns var(--dur-std) var(--ease-out);
	}

	.shell.collapsed {
		--nav-width: 56px;
	}

	.sidebar {
		width: var(--nav-width);
		min-width: var(--nav-width);
		height: 100%;
		background: var(--canvas-raised);
		border-right: 1px solid var(--border-subtle);
		display: flex;
		flex-direction: column;
		padding: var(--space-4) 0;
		overflow: hidden;
	}

	.sidebar-brand {
		padding: var(--space-3) var(--space-4) var(--space-4);
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	/* Collapsed: center the brand mark, drop its horizontal padding so the 22px
	   mark sits centered in the 56px rail. The name hides but the toggle stays
	   (as its own centered row below) so the rail can always be re-expanded. */
	.collapsed .sidebar-brand {
		padding: var(--space-3) 0 var(--space-3);
		flex-direction: column;
		gap: var(--space-3);
		justify-content: center;
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
		flex: 1;
		min-width: 0;
		white-space: nowrap;
		overflow: hidden;
	}

	/* Collapse toggle: sits at the brand row's right edge when expanded, and on
	   its own centered row under the brand mark when collapsed — always present so
	   the rail can be re-expanded. */
	.collapse-btn {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		flex-shrink: 0;
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--text-tertiary);
		cursor: pointer;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.collapse-btn:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	.collapsed .brand-name {
		display: none;
	}

	/* Collapsed: give the toggle a faint boxed look so it reads as the "expand"
	   affordance rather than blending into the rail. */
	.collapsed .collapse-btn {
		border: 1px solid var(--border-subtle);
	}

	.collapsed .collapse-btn:hover {
		border-color: var(--border-default);
	}

	.sidebar-section {
		padding: var(--space-3) var(--space-3) var(--space-1);
		display: flex;
		flex-direction: column;
		gap: 1px;
	}

	.collapsed .sidebar-section {
		padding: var(--space-3) var(--space-2) var(--space-1);
	}

	.nav-item {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 6px var(--space-2);
		border-radius: var(--radius-sm);
		color: var(--text-secondary);
		font-size: 12.5px;
		font-weight: 450;
		font-family: inherit;
		text-align: left;
		width: 100%;
		border: none;
		background: transparent;
		cursor: pointer;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
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

	.nav-icon {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 18px;
		height: 18px;
		flex-shrink: 0;
		color: var(--text-tertiary);
		transition: color var(--dur-fast) var(--ease-out);
	}

	.nav-item:hover .nav-icon,
	.nav-item.active .nav-icon {
		color: var(--text-primary);
	}

	.nav-item.active .nav-icon {
		color: var(--accent-ink);
	}

	.nav-label {
		min-width: 0;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	/* Collapsed: labels vanish, each row centers its icon in the rail. */
	.collapsed .nav-item {
		justify-content: center;
		padding: 6px 0;
	}

	.collapsed .nav-label {
		display: none;
	}

	.sidebar-spacer {
		flex: 1;
	}

	.sidebar-bottom {
		padding: var(--space-3) var(--space-3) 0;
		border-top: 1px solid var(--border-subtle);
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 1px;
		margin-top: var(--space-3);
	}

	.collapsed .sidebar-bottom {
		padding: var(--space-3) var(--space-2) 0;
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
		.shell,
		.shell.collapsed {
			grid-template-columns: 1fr;
			grid-template-rows: auto 1fr;
			--nav-width: 100%;
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
		/* The collapse toggle is desktop-only; on the horizontal mobile bar the
		   labels always show. */
		.collapse-btn {
			display: none;
		}
		.sidebar-section {
			display: flex;
			align-items: center;
			gap: var(--space-1);
			padding: 0;
			flex-direction: row;
		}
		.sidebar-spacer {
			display: none;
		}
		.sidebar-bottom {
			border-top: none;
			margin-top: 0;
			padding: 0;
			flex-direction: row;
			align-items: center;
		}
	}

	/* Scrollbar — cross-browser (Edge Fluent: color only; Chrome/Safari: full control) */
	:global(html) {
		scrollbar-width: thin;
		scrollbar-color: var(--canvas-float) var(--canvas-overlay);
	}
</style>
