<script lang="ts">
	import { fade, scale } from 'svelte/transition';
	import { fadeIn, pop } from '$lib/motion';
	import { client } from '$lib/client';
	import Button from './Button.svelte';
	import PermissionRulesEditor from './PermissionRulesEditor.svelte';
	import LspConfigEditor from './LspConfigEditor.svelte';
	import FormatConfigEditor from './FormatConfigEditor.svelte';
	import Skeleton from './Skeleton.svelte';
	import { lspToRows, lspFromRows, fmtToRows, fmtFromRows, type LspRow, type FmtRow } from '$lib/lang-tools';
	import type { WorkspaceConfig } from '$lib/types/WorkspaceConfig';
	import type { ToolInfo } from '$lib/types/ToolInfo';
	import type { FormatMode } from '$lib/types/FormatMode';

	/** Per-workspace config editor (`doc/workspace-config.md`): the **top** tier
	 *  of the permission gate plus the network override, AND the workspace-local
	 *  LSP/format checklists (the project-level overrides written to the
	 *  workspace's own `.omini`, `doc/lsp.md` §8 / `doc/format.md` §8). Install
	 *  probes here run against the workspace's env-overlay PATH, so a
	 *  flake/direnv-provided tool shows as installed. Loads on mount, saves full
	 *  desired state. */
	let { workspaceId, onclose }: { workspaceId: string; onclose: () => void } = $props();

	let config = $state<WorkspaceConfig | null>(null);
	let error = $state<string | null>(null);
	let saving = $state(false);
	let toolCatalog = $state<ToolInfo[]>([]);

	// Workspace-local LSP/format checklists. Rows bind for editing; the dirty
	// signal is the serialized PUT body (rows carry display-only fields).
	let lspRows = $state<LspRow[]>([]);
	let lspSnapshot = $state<string | null>(null);
	let lspLoaded = $state(false);
	let fmtRows = $state<FmtRow[]>([]);
	let fmtMode = $state<FormatMode>('file');
	let fmtSnapshot = $state<string | null>(null);
	let fmtLoaded = $state(false);

	// Network policy is a small enum; `null` = inherit profile/gateway. The
	// allowlist hosts are edited only when the policy is `allowlist`.
	const NETWORK_POLICIES = [
		{ value: '', label: 'Inherit (profile / gateway)' },
		{ value: 'isolated', label: 'isolated (no network)' },
		{ value: 'allowlist', label: 'allowlist (listed hosts only)' },
		{ value: 'open', label: 'open (unrestricted)' }
	];

	let panelEl = $state<HTMLDivElement | null>(null);
	// Snapshot the config as first loaded, to detect unsaved edits on close.
	let initialSnapshot = $state<string | null>(null);
	$effect(() => {
		load();
	});
	// Focus the panel once it exists so Escape is captured and the dialog is
	// keyboard-reachable (mirrors ConfirmDialog).
	$effect(() => {
		panelEl?.focus();
	});

	// Whether the user has unsaved edits (config diverged from the loaded snapshot).
	const configDirty = $derived(
		initialSnapshot !== null && config !== null && JSON.stringify(config) !== initialSnapshot
	);
	const lspDirty = $derived(
		lspSnapshot !== null && JSON.stringify(lspFromRows(lspRows)) !== lspSnapshot
	);
	const fmtDirty = $derived(
		fmtSnapshot !== null && JSON.stringify(fmtFromRows(fmtRows, fmtMode)) !== fmtSnapshot
	);
	const dirty = $derived(configDirty || lspDirty || fmtDirty);

	async function load() {
		error = null;
		try {
			// This workspace's tool catalog: built-ins + its MCP tools (best-effort;
			// MCP failures degrade to built-ins server-side). Drives the per-tool cards.
			const [loaded, tools] = await Promise.all([
				client.getWorkspaceConfig(workspaceId),
				client.listWorkspaceTools(workspaceId)
			]);
			// The backend omits empty sections (skip_serializing_if), so a never
			// -configured workspace arrives as `{}` — the optional fields are then
			// `undefined`. Normalize before binding so PermissionEditor / the network
			// controls always have a concrete shape (the TS type over-promises here).
			loaded.permission ??= {};
			loaded.mounts ??= [];
			loaded.network ??= null;
			config = loaded;
			initialSnapshot = JSON.stringify(loaded);
			toolCatalog = tools;
			// The workspace-local checklists load alongside (independent of the
			// network/permission config). Each seeds its rows + dirty baseline.
			client
				.getWorkspaceLspConfig(workspaceId)
				.then((v) => {
					lspRows = lspToRows(v);
					lspSnapshot = JSON.stringify(lspFromRows(lspRows));
					lspLoaded = true;
				})
				.catch(() => {
					lspLoaded = true; // a failed probe renders as an empty note, not a hang
				});
			client
				.getWorkspaceFormatConfig(workspaceId)
				.then((v) => {
					fmtRows = fmtToRows(v);
					fmtMode = v.mode;
					fmtSnapshot = JSON.stringify(fmtFromRows(fmtRows, fmtMode));
					fmtLoaded = true;
				})
				.catch(() => {
					fmtLoaded = true;
				});
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	// The network `<select>` binds a plain string; map to/from the nullable
	// section. Setting a policy on an absent section materializes it.
	function networkPolicy(): string {
		return config?.network?.policy ?? '';
	}
	function setNetworkPolicy(v: string) {
		if (!config) return;
		if (v === '') {
			// Inherit: drop the whole section so it doesn't count as an override.
			config.network = null;
			return;
		}
		const allow = config.network?.allow ?? [];
		config.network = { policy: v, allow };
	}
	function allowLines(): string {
		return (config?.network?.allow ?? []).join('\n');
	}
	function setAllowLines(v: string) {
		if (!config?.network) return;
		config.network.allow = v
			.split('\n')
			.map((s) => s.trim())
			.filter((s) => s.length > 0);
	}

	async function save() {
		if (!config) return;
		saving = true;
		error = null;
		try {
			// Save each dirty unit; stop at the first failure so the user sees
			// which one failed (already-saved units stay saved).
			if (configDirty) await client.saveWorkspaceConfig(workspaceId, config);
			if (lspDirty) await client.saveWorkspaceLspConfig(workspaceId, lspFromRows(lspRows));
			if (fmtDirty) await client.saveWorkspaceFormatConfig(workspaceId, fmtFromRows(fmtRows, fmtMode));
			onclose();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			saving = false;
		}
	}

	// One-shot "unsaved changes" guard: a dirty dialog's first close request is
	// intercepted (a warning shows); the next confirms. A clean dialog closes at
	// once. This is the low-cost protection against a misclick discarding a lot of
	// exception editing.
	let confirmingClose = $state(false);
	function attemptClose() {
		if (dirty && !confirmingClose) {
			confirmingClose = true;
			return;
		}
		onclose();
	}

	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.stopPropagation();
			attemptClose();
		}
	}
</script>

<svelte:window {onkeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="backdrop" role="presentation" onclick={attemptClose} transition:fade={fadeIn()}>
	<div
		bind:this={panelEl}
		class="panel"
		role="dialog"
		aria-modal="true"
		aria-label="Workspace configuration"
		tabindex="-1"
		onclick={(e) => e.stopPropagation()}
		transition:scale={pop()}
	>
		<h2 class="title">Workspace configuration</h2>
		<p class="sub">
			The top gating tier (deny unions with profile / gateway, never shrinks) + network overrides.
			Stored in the gateway’s trusted directory.
		</p>

		{#if config}
			<div class="body">
				<div class="net">
					<span class="key">Network policy</span>
					<select
						class="in sel"
						value={networkPolicy()}
						onchange={(e) => setNetworkPolicy(e.currentTarget.value)}
					>
						{#each NETWORK_POLICIES as p (p.value)}
							<option value={p.value}>{p.label}</option>
						{/each}
					</select>
					{#if networkPolicy() === 'allowlist'}
						<label class="net-allow">
							<span class="key">Allowed hosts (one per line)</span>
							<textarea
								class="in ta"
								rows="2"
								value={allowLines()}
								oninput={(e) => setAllowLines(e.currentTarget.value)}
								placeholder={'crates.io\npypi.org'}
								spellcheck="false"
							></textarea>
						</label>
					{/if}
				</div>

				{#if config.permission}
					<div class="perm-block">
						<span class="key">Permission gating</span>
						<PermissionRulesEditor bind:policy={config.permission} tools={toolCatalog} />
					</div>
				{/if}

				<!-- Workspace-local LSP/format: the project-level overrides. Install
				     probes run against THIS workspace's env (flake/direnv), so these
				     reflect what a session here can actually spawn. -->
				<div class="perm-block">
					<span class="key">语言服务器（本项目）</span>
					{#if lspLoaded && lspRows.length > 0}
						<LspConfigEditor bind:rows={lspRows} />
					{:else if lspLoaded}
						<p class="none-note">无法加载语言服务器配置。</p>
					{:else}
						<Skeleton width="100%" height="44px" radius="var(--radius-md)" />
					{/if}
				</div>

				<div class="perm-block">
					<span class="key">格式化（本项目）</span>
					{#if fmtLoaded && fmtRows.length > 0}
						<FormatConfigEditor bind:rows={fmtRows} bind:mode={fmtMode} />
					{:else if fmtLoaded}
						<p class="none-note">无法加载格式化配置。</p>
					{:else}
						<Skeleton width="100%" height="44px" radius="var(--radius-md)" />
					{/if}
				</div>
			</div>
		{:else if !error}
			<div class="body" aria-hidden="true">
				<div class="net">
					<Skeleton width="72px" height="10px" />
					<Skeleton width="100%" height="30px" />
				</div>
				<div class="skel-cards">
					{#each Array(3) as _}
						<Skeleton width="100%" height="64px" radius="var(--radius-md)" />
					{/each}
				</div>
			</div>
		{/if}

		{#if error}
			<p class="error" role="alert">{error}</p>
		{/if}

		{#if confirmingClose}
			<p class="warn" role="alert">
				There are unsaved changes; clicking “Discard” again will drop them.
			</p>
		{/if}
		<div class="actions">
			{#if confirmingClose}
				<Button variant="danger" onclick={onclose}>Discard changes</Button>
				<Button variant="ghost" onclick={() => (confirmingClose = false)}>Keep editing</Button>
			{:else}
				<Button variant="ghost" onclick={attemptClose}>Cancel</Button>
			{/if}
			<Button variant="accent" onclick={save} disabled={saving || !config}>
				{saving ? 'Saving…' : 'Save'}
			</Button>
		</div>
	</div>
</div>

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		z-index: var(--z-modal);
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--backdrop);
		padding: var(--space-4);
	}

	.panel {
		background: var(--canvas-raised);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-lg);
		padding: var(--space-5);
		max-width: 560px;
		width: 100%;
		max-height: 84vh;
		overflow-y: auto;
		box-shadow: var(--shadow-lg);
	}

	.title {
		margin: 0 0 var(--space-1);
		font-size: 15px;
		font-weight: 600;
		color: var(--text-primary);
		font-family: var(--font-chinese);
	}

	.sub {
		margin: 0 0 var(--space-4);
		font-size: 12px;
		line-height: 1.5;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.body {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		margin-bottom: var(--space-4);
	}

	.net {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.net-allow {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}

	.perm-block {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.none-note {
		font-size: 11px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.key {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.in {
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

	.in:focus {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}

	.ta {
		resize: vertical;
		line-height: 1.5;
	}

	.error {
		margin: 0 0 var(--space-4);
		font-size: 13px;
		color: var(--state-error-text);
		font-family: var(--font-chinese);
	}

	.skel-cards {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.sel {
		font-family: var(--font-chinese);
		background: var(--canvas-raised);
	}

	.warn {
		margin: 0 0 var(--space-3);
		font-size: 12px;
		color: var(--state-running-text);
		font-family: var(--font-chinese);
	}

	.actions {
		display: flex;
		justify-content: flex-end;
		gap: var(--space-2);
	}
</style>
