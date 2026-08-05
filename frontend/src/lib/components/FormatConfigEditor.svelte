<script lang="ts">
	import { layerLabel, type FmtRow } from '$lib/lang-tools';
	import type { FormatMode } from '$lib/types/FormatMode';
	import PickerSelect from '$lib/components/PickerSelect.svelte';
	import { type SelectOption } from '$lib/components/ModelSelect.svelte';

	/** Registry-driven checklist editor for the layered format config
	 *  (`doc/format.md` §7): the global `mode` (file/edit/off) plus every
	 *  registry formatter as a fixed checklist (a tombstoned one stays
	 *  visible, greyed). Same shape as `LspConfigEditor` — NOT the permission
	 *  editor's incremental list.
	 *
	 *  `rows` and `mode` are two-way bound for the parent's dirty tracking; the
	 *  parent seeds them from the view (`fmtToRows` + `view.mode`) and re-seeds
	 *  on re-fetch, so this component only renders and mutates. The row ⇄ wire
	 *  mapping lives in the tested pure module `lang-tools.ts`.
	 *
	 *  `showInstalled` gates the install badge + the not-installed command lock
	 *  (truthful only in a workspace view; the global view passes `false` — see
	 *  `LspConfigEditor`). */
	let {
		rows = $bindable(),
		mode = $bindable(),
		showInstalled = true
	}: { rows: FmtRow[]; mode: FormatMode; showInstalled?: boolean } = $props();

	const MODE_OPTIONS: SelectOption[] = [
		{ value: 'file', label: 'file', detail: '整文件格式化（默认，最稳定）' },
		{ value: 'edit', label: 'edit', detail: '只格式化本次编辑的行段' },
		{ value: 'off', label: 'off', detail: '禁用自动格式化' }
	];

	// The trigger shows the current mode (the options' labels are the values).
	const modeLabel = $derived(MODE_OPTIONS.find((o) => o.value === mode)?.label ?? mode);

	function commandEditable(row: FmtRow): boolean {
		return !showInstalled || row.installed;
	}
</script>

<div class="cfg-editor">
	<div class="mode-row">
		<span class="key">mode</span>
		<PickerSelect options={MODE_OPTIONS} bind:value={mode} key="mode" label={modeLabel} />
		{#if mode === 'edit'}
			<p class="mode-hint">不支持局部格式化的 formatter 在 edit 模式下跳过（绝不静默回退整文件）。</p>
		{/if}
	</div>

	{#each rows as row (row.name)}
		<div class="row" class:off={!row.enabled} class:not-installed={showInstalled && !row.installed}>
			<label class="toggle" title={row.enabled ? '点击禁用（写入 enabled = false 墓碑）' : '点击启用'}>
				<input type="checkbox" bind:checked={row.enabled} />
				<span class="track" aria-hidden="true"></span>
			</label>

			<div class="row-main">
				<div class="row-top">
					<span class="name">{row.name}</span>
					<span class="badge layer">{layerLabel(row.layer, row.builtin)}</span>
					{#if showInstalled}
						{#if row.installed}
							<span class="badge installed">已安装</span>
						{:else}
							<span class="badge missing">未安装</span>
						{/if}
					{/if}
					{#if row.supportsLineRange}
						<span class="badge range">局部</span>
					{/if}
				</div>
				<div class="row-sub">
					<span class="mono dim">{row.command}{row.args.length > 0 ? ' ' + row.args.join(' ') : ''}</span>
					<span class="exts mono">{row.extensions.map((e) => '.' + e).join(' ')}</span>
				</div>
				{#if commandEditable(row)}
					<label class="cmd-field">
						<span class="key">command</span>
						<input class="in mono" bind:value={row.command} spellcheck="false" />
					</label>
				{:else}
					<p class="cmd-hint">未安装的二进制不可改 command —— 先安装，或禁用此条目。</p>
				{/if}
			</div>
		</div>
	{:else}
		<p class="empty">无可用格式化器。</p>
	{/each}
</div>

<style>
	.cfg-editor {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.mode-row {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-3);
		margin-bottom: var(--space-2);
		border-bottom: 1px solid var(--border-subtle);
	}

	.mode-hint {
		font-family: var(--font-chinese);
		font-size: 11px;
		color: var(--text-tertiary);
	}

	.row {
		display: flex;
		align-items: flex-start;
		gap: var(--space-3);
		padding: var(--space-3);
		border: 1px solid transparent;
		border-radius: var(--radius-md);
	}

	.row:hover {
		background: var(--surface-hover);
	}

	.row.off,
	.row.not-installed {
		opacity: 0.55;
	}

	.row-main {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}

	.row-top {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.name {
		font-family: var(--font-mono);
		font-size: 13px;
		font-weight: 510;
		color: var(--text-primary);
	}

	.row.off .name,
	.row.not-installed .name {
		color: var(--text-secondary);
	}

	.badge {
		flex-shrink: 0;
		font-family: var(--font-chinese);
		font-size: 11px;
		font-weight: 590;
		padding: 1px 6px;
		border-radius: var(--radius-sm);
	}

	.badge.layer {
		color: var(--text-tertiary);
		background: var(--canvas-float);
	}

	.badge.installed {
		color: var(--state-done-text);
		background: var(--state-done-bg);
	}

	.badge.missing {
		color: var(--text-tertiary);
		background: var(--canvas-float);
	}

	.badge.range {
		color: var(--state-running-text);
		background: var(--state-running-bg);
	}

	.row-sub {
		display: flex;
		align-items: baseline;
		gap: var(--space-3);
		min-width: 0;
	}

	.mono {
		font-family: var(--font-mono);
		font-size: 11.5px;
	}

	.dim {
		color: var(--text-tertiary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.exts {
		flex-shrink: 0;
		color: var(--text-tertiary);
	}

	.cmd-field {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin-top: 2px;
	}

	.key {
		flex-shrink: 0;
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.in {
		flex: 1;
		min-width: 0;
		padding: 4px 8px;
		background: var(--canvas-base);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		color: var(--text-primary);
		font-size: 12px;
		outline: none;
	}

	.in:focus {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}

	.cmd-hint {
		font-family: var(--font-chinese);
		font-size: 11px;
		color: var(--text-disabled);
	}

	.empty {
		font-family: var(--font-chinese);
		font-size: 12px;
		color: var(--text-tertiary);
		padding: var(--space-2) 0;
	}

	/* ---- Enable toggle (pure CSS switch, token-colored) ---- */
	.toggle {
		position: relative;
		flex-shrink: 0;
		width: 30px;
		height: 17px;
		margin-top: 2px;
		cursor: pointer;
	}

	.toggle input {
		position: absolute;
		opacity: 0;
		width: 100%;
		height: 100%;
		margin: 0;
		cursor: pointer;
	}

	.track {
		position: absolute;
		inset: 0;
		background: var(--canvas-float);
		border: 1px solid var(--border-default);
		border-radius: 999px; /* pill: state-toggle affordance (§5 allows full radius here) */
		transition:
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
		pointer-events: none;
	}

	.track::after {
		content: '';
		position: absolute;
		top: 2px;
		left: 2px;
		width: 11px;
		height: 11px;
		border-radius: 50%;
		background: var(--text-tertiary);
		transition:
			transform var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.toggle input:checked + .track {
		background: var(--accent-dim);
		border-color: var(--accent);
	}

	.toggle input:checked + .track::after {
		transform: translateX(13px);
		background: var(--accent);
	}

	.toggle input:focus-visible + .track {
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}
</style>
