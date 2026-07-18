<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { fly } from 'svelte/transition';
	import { client } from '$lib/client';
	import { rise } from '$lib/motion';
	import Skeleton from '$lib/components/Skeleton.svelte';
	import type { WorkspaceSummary } from '$lib/types/WorkspaceSummary';

	let workspaces = $state<WorkspaceSummary[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	// "New workspace" popover: an absolute path to open. We have no filesystem
	// picker yet, so the user types the path; the gateway validates it exists and
	// returns the id to route to.
	let cfgOpen = $state(false);
	let newPath = $state('');
	let creating = $state(false);
	let createError = $state<string | null>(null);

	async function refresh() {
		loading = true;
		error = null;
		try {
			workspaces = await client.listWorkspaces();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}

	/** Register the typed path as a workspace and open its panel. The gateway
	 *  rejects a non-existent path (400) — surfaced inline, not thrown away. */
	async function createWorkspace() {
		const path = newPath.trim();
		if (!path || creating) return;
		creating = true;
		createError = null;
		try {
			const id = await client.createWorkspace(path);
			cfgOpen = false;
			newPath = '';
			void goto(`/workspaces/${id}`);
		} catch (e) {
			createError = e instanceof Error ? e.message : String(e);
		} finally {
			creating = false;
		}
	}

	function onPathKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter') {
			e.preventDefault();
			void createWorkspace();
		} else if (e.key === 'Escape') {
			cfgOpen = false;
		}
	}

	/** Focus the popover's path input when it mounts (the a11y-safe equivalent
	 *  of the autofocus attribute, which svelte-check flags). */
	function focusOnMount(node: HTMLInputElement) {
		node.focus();
	}

	function formatTime(iso: string | null): string {
		if (!iso) return '—';
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

	/** Workspace display name: the last path segment, or "无工作区" for the
	 *  no-workspace group (path is null there). */
	function wsName(ws: WorkspaceSummary): string {
		if (!ws.path) return '无工作区';
		return ws.path.split('/').filter(Boolean).pop() ?? ws.path;
	}

	/** Shorter path for the card subtitle: keep the last two segments. */
	function wsPathLabel(path: string): string {
		const parts = path.split('/').filter(Boolean);
		return parts.length > 2 ? '…/' + parts.slice(-2).join('/') : path;
	}

	function sessionCountLabel(n: number): string {
		return n === 1 ? '1 个会话' : `${n} 个会话`;
	}

	onMount(() => {
		void refresh();
	});
</script>

<div class="page">
	<div class="page-inner">
		<header>
			<h1>Workspaces</h1>
			<div class="newbtn" class:open={cfgOpen}>
				<button class="newbtn-main" onclick={() => (cfgOpen = !cfgOpen)} aria-expanded={cfgOpen}>
					New workspace
				</button>
				{#if cfgOpen}
					<!-- Click-away scrim: an invisible full-viewport layer behind the
					     popover (z-30) that closes it on any outside click. Esc is handled
					     on the path input, which takes focus on open. -->
					<!-- svelte-ignore a11y_click_events_have_key_events -->
					<div class="popover-scrim" role="presentation" onclick={() => (cfgOpen = false)}></div>
					<div class="newbtn-popover" transition:fly={rise(-6)}>
						<label class="cfg-field">
							<span class="cfg-key">Workspace path</span>
							<input
								class="cfg-input"
								type="text"
								bind:value={newPath}
								onkeydown={onPathKeydown}
								placeholder="/absolute/path/to/project"
								spellcheck="false"
								use:focusOnMount
							/>
						</label>
						{#if createError}
							<p class="cfg-error">{createError}</p>
						{/if}
						<button
							class="newbtn-go"
							disabled={!newPath.trim() || creating}
							onclick={createWorkspace}
						>
							{creating ? '创建中…' : '打开工作区'}
						</button>
					</div>
				{/if}
			</div>
		</header>

		{#if error}
			<p class="error">{error}</p>
		{/if}

		{#if loading}
			<ul class="grid" aria-hidden="true">
				{#each Array(6) as _}
					<li class="skel-card">
						<Skeleton width="55%" height="14px" />
						<Skeleton width="38%" height="11px" />
						<div class="skel-foot">
							<Skeleton width="56px" height="12px" />
							<Skeleton width="44px" height="11px" />
						</div>
					</li>
				{/each}
			</ul>
		{:else if workspaces.length === 0}
			<div class="empty" in:fly={rise(8)}>
				<svg
					width="36"
					height="36"
					viewBox="0 0 36 36"
					fill="none"
					stroke="currentColor"
					stroke-width="1.4"
					stroke-linecap="round"
					stroke-linejoin="round"
					aria-hidden="true"
				>
					<path
						d="M5 11a2 2 0 0 1 2-2h6.5l2.5 3H31a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V11z"
					/>
					<line x1="19" y1="17" x2="19" y2="23" />
					<line x1="16" y1="20" x2="22" y2="20" />
				</svg>
				<p class="empty-title">还没有工作区</p>
				<p class="empty-sub">工作区是一个项目目录，会话都在其中运行。</p>
				<button class="empty-cta" onclick={() => (cfgOpen = true)}>新建工作区</button>
			</div>
		{:else}
			<ul class="grid">
				{#each workspaces as ws, i (ws.id)}
					<li in:fly={rise(10, 200, Math.min(i * 40, 200))}>
						<a href={`/workspaces/${ws.id}`} class="card">
							<div class="card-head">
								<div class="card-title">{wsName(ws)}</div>
								{#if ws.path}
									<div class="card-path" title={ws.path}>{wsPathLabel(ws.path)}</div>
								{/if}
							</div>
							<div class="card-foot">
								<span class="count">{sessionCountLabel(ws.session_count)}</span>
								<span class="time">{formatTime(ws.latest_session_time)}</span>
							</div>
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

	/* ---- NEW WORKSPACE BUTTON + POPOVER ---- */
	.newbtn {
		position: relative;
		display: inline-flex;
		align-items: stretch;
		border-radius: var(--radius-md);
	}

	.newbtn-main {
		background: var(--accent);
		color: var(--accent-fg);
		border: 1px solid var(--accent);
		font-size: 14px;
		font-weight: 590;
		cursor: pointer;
		padding: var(--gap-sm) var(--gap-lg);
		border-radius: var(--radius-md);
		transition:
			background var(--motion-fast),
			border-color var(--motion-fast);
	}

	.newbtn-main:hover,
	.newbtn.open .newbtn-main {
		background: var(--accent-hover);
		border-color: var(--accent-hover);
	}

	.newbtn-popover {
		position: absolute;
		top: calc(100% + 6px);
		right: 0;
		z-index: var(--z-popover);
		width: 320px;
		max-width: min(320px, 80vw);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		padding: var(--space-4);
		background: var(--canvas-overlay);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-md, 0 8px 24px rgba(0, 0, 0, 0.18));
	}

	.popover-scrim {
		position: fixed;
		inset: 0;
		z-index: calc(var(--z-popover) - 1);
		cursor: default;
	}

	.cfg-field {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}

	.cfg-key {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.cfg-input {
		width: 100%;
		padding: 6px 8px;
		background: var(--canvas-base);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		color: var(--text-primary);
		font-family: var(--font-mono);
		font-size: 12px;
		outline: none;
	}

	.cfg-input:focus {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}

	.cfg-input::placeholder {
		color: var(--text-disabled);
	}

	.cfg-error {
		font-size: 11px;
		color: var(--state-error-text);
		background: var(--state-error-bg);
		padding: var(--space-2);
		border-radius: var(--radius-sm);
		line-height: 1.4;
	}

	.newbtn-go {
		margin-top: 2px;
		padding: 7px 10px;
		background: var(--accent);
		color: var(--accent-fg);
		border: 1px solid var(--accent);
		border-radius: var(--radius-sm);
		font-size: 13px;
		font-weight: 590;
		cursor: pointer;
		transition: background var(--motion-fast);
	}

	.newbtn-go:hover {
		background: var(--accent-hover);
	}

	.newbtn-go:disabled {
		opacity: 0.55;
		cursor: not-allowed;
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

	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-12) var(--space-4);
		color: var(--text-disabled);
	}

	.empty-title {
		font-size: 15px;
		font-weight: 590;
		color: var(--text-secondary);
		font-family: var(--font-chinese);
		margin-top: var(--space-2);
	}

	.empty-sub {
		font-size: 13px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.empty-cta {
		margin-top: var(--space-3);
		padding: var(--space-2) var(--space-4);
		background: var(--canvas-overlay);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-md);
		color: var(--text-primary);
		font-size: 13px;
		font-weight: 590;
		font-family: var(--font-chinese);
		cursor: pointer;
		transition:
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
	}

	.empty-cta:hover {
		background: var(--surface-hover);
		border-color: var(--border-strong);
	}

	/* ---- WORKSPACE GRID ---- */
	.grid {
		list-style: none;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
		gap: var(--space-3);
	}

	.grid li {
		min-width: 0;
	}

	.skel-card {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		padding: var(--space-4);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
	}

	.skel-foot {
		display: flex;
		justify-content: space-between;
		padding-top: var(--space-3);
		border-top: 1px solid var(--border-subtle);
		margin-top: var(--space-2);
	}

	.card {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		min-width: 0;
		height: 100%;
		padding: var(--space-4);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-raised);
		text-decoration: none;
		transition:
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out),
			transform var(--dur-fast) var(--ease-out);
	}

	.card:hover {
		background: var(--surface-hover);
		border-color: var(--border-strong);
		transform: translateY(-1px);
	}

	.card-head {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 0;
	}

	.card-title {
		color: var(--text-primary);
		font-weight: 590;
		font-size: 14px;
		line-height: 1.4;
		font-family: var(--font-chinese);
		min-width: 0;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.card-path {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-tertiary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		min-width: 0;
	}

	.card-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
		padding-top: var(--space-3);
		border-top: 1px solid var(--border-subtle);
		margin-top: auto;
	}

	.count {
		font-size: 12px;
		font-weight: 510;
		color: var(--text-secondary);
	}

	.time {
		color: var(--text-tertiary);
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}
</style>
