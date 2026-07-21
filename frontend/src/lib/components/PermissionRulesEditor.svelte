<script lang="ts">
	import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
	import type { ToolInfo } from '$lib/types/ToolInfo';
	import type { MatchMode } from '$lib/types/MatchMode';
	import type { Rule } from '$lib/types/Rule';
	import { toRows, fromRows, summaryOf, type RuleRow, type Decision } from '$lib/permission-rules';

	/** Incremental rule-list editor for a {@link PermissionPolicy} at any tier
	 *  (profile / gateway / workspace, `doc/permission.md` §3). Only the rules the
	 *  user actually added are rendered — an empty tier is an empty list with an
	 *  "add rule" button, never a full per-tool list. Conditions (field / mode /
	 *  allow-list) start collapsed; complexity grows only when the user asks for
	 *  it. The row ⇄ policy mapping lives in the tested pure module
	 *  `permission-rules.ts`; this component only renders.
	 *
	 *  Reseeding on tier/profile/workspace switch is the parent's job: wrap the
	 *  editor in `{#key ...}` so a new subject gets a fresh component. */
	let {
		policy = $bindable(),
		tools = [],
		showDefaults = false,
		emptyHint = '无规则 —— 继承下层，未命中即允许'
	}: {
		policy: PermissionPolicy;
		tools?: ToolInfo[];
		/** Show the collapsible per-tool default table (gateway baseline only). */
		showDefaults?: boolean;
		emptyHint?: string;
	} = $props();

	// Editor-local row: the serializable RuleRow plus UI-only state (accordion
	// open, condition section open) that never touches the disk model.
	interface EditRow extends RuleRow {
		open: boolean;
		cond: boolean;
	}

	const seeded = toRows(policy);
	let rows = $state<EditRow[]>(
		seeded.rows.map((r) => ({ ...r, open: false, cond: hasCondition(r) }))
	);
	let advanced = $state<{ list: Decision; rule: Rule }[]>(seeded.advanced);

	function hasCondition(r: RuleRow): boolean {
		return r.values.length > 0 || r.negate;
	}

	// Push row edits back to the bound policy. Called after every mutation.
	// All three lists are written back — dropping `allow` would silently lose
	// pinned approvals on save (and make allow-only edits invisible to the
	// parent's dirty snapshot).
	function commit() {
		const next = fromRows({ rows, advanced });
		policy.deny = next.deny;
		policy.allow = next.allow;
		policy.ask = next.ask;
	}

	// ---- Rule rows ----

	// Rows rendered in the list. With the defaults table on, bare rows for
	// catalog tools live in the table instead — showing them twice would let
	// the two editors fight. Bare rows for "*" / non-catalog tools stay here.
	const listRows = $derived(
		showDefaults
			? rows.filter(
					(r) => hasCondition(r) || r.tool === '*' || !tools.some((t) => t.name === r.tool)
				)
			: rows
	);

	function addRow() {
		const tool = tools[0]?.name ?? '*';
		rows.push({
			list: 'ask',
			tool,
			field: null,
			mode: 'substring',
			negate: false,
			values: [],
			open: true,
			cond: false
		});
		commit();
	}

	function removeRow(row: EditRow) {
		rows = rows.filter((r) => r !== row);
		commit();
	}

	function toggleOpen(row: EditRow) {
		const next = !row.open;
		// Accordion: one open editor at a time keeps the list scannable.
		for (const r of rows) r.open = false;
		row.open = next;
	}

	function fieldsOf(tool: string) {
		return tools.find((t) => t.name === tool)?.fields ?? [];
	}

	// Picking a path field defaults to prefix mode (directory lists); others to
	// substring — the common case needs no dropdown fiddling.
	function onFieldChange(row: EditRow, key: string) {
		row.field = key || null;
		const f = fieldsOf(row.tool).find((f) => f.key === row.field);
		row.mode = f?.is_path ? 'prefix' : 'substring';
		commit();
	}

	function onToolChange(row: EditRow, tool: string) {
		row.tool = tool;
		// The old field may not exist on the new tool; fall back to its first.
		const fields = fieldsOf(tool);
		if (row.field && !fields.some((f) => f.key === row.field)) {
			row.field = null;
		}
		commit();
	}

	function openCondition(row: EditRow) {
		row.cond = true;
		const fields = fieldsOf(row.tool);
		if (!row.field && fields.length > 0) {
			row.field = fields[0].key;
			row.mode = fields[0].is_path ? 'prefix' : 'substring';
		}
		commit();
	}

	function closeCondition(row: EditRow) {
		row.cond = false;
		row.field = null;
		row.mode = 'substring';
		row.negate = false;
		row.values = [];
		commit();
	}

	// values <-> textarea: one pattern per non-empty line.
	function linesToValues(v: string): string[] {
		return v
			.split('\n')
			.map((s) => s.trim())
			.filter((s) => s.length > 0);
	}

	// ---- Tool defaults table (gateway baseline) ----

	type DefaultVerdict = 'allow' | 'ask' | 'deny';

	let defaultsOpen = $state(false);

	function isBare(r: EditRow): boolean {
		return !hasCondition(r);
	}

	function defaultOf(tool: string): DefaultVerdict {
		// deny outranks ask when both bare rules somehow exist.
		if (rows.some((r) => r.tool === tool && isBare(r) && r.list === 'deny')) return 'deny';
		if (rows.some((r) => r.tool === tool && isBare(r) && r.list === 'ask')) return 'ask';
		return 'allow';
	}

	function setDefault(tool: string, d: DefaultVerdict) {
		rows = rows.filter((r) => !(r.tool === tool && isBare(r)));
		if (d !== 'allow') {
			rows.push({
				list: d,
				tool,
				field: null,
				mode: 'substring',
				negate: false,
				values: [],
				open: false,
				cond: false
			});
		}
		commit();
	}

	const defaultsSetCount = $derived(tools.filter((t) => defaultOf(t.name) !== 'allow').length);

	const DEFAULTS: { value: DefaultVerdict; label: string }[] = [
		{ value: 'allow', label: '允许' },
		{ value: 'ask', label: '询问' },
		{ value: 'deny', label: '拒绝' }
	];
</script>

<div class="rules-editor">
	{#if showDefaults && tools.length > 0}
		<div class="defaults">
			<button
				class="defaults-head"
				onclick={() => (defaultsOpen = !defaultsOpen)}
				aria-expanded={defaultsOpen}
			>
				<svg
					class="chev"
					class:open={defaultsOpen}
					viewBox="0 0 14 14"
					fill="none"
					stroke="currentColor"
					stroke-width="1.6"
					stroke-linecap="round"
					stroke-linejoin="round"
				>
					<polyline points="5,3 9,7 5,11" />
				</svg>
				<span>工具默认</span>
				<span class="defaults-count">
					{defaultsSetCount > 0 ? `${defaultsSetCount} 项已设置` : '全部允许'}
				</span>
			</button>
			{#if defaultsOpen}
				<div class="defaults-body">
					{#each tools as t (t.name)}
						{@const cur = defaultOf(t.name)}
						<div class="defaults-row">
							<span class="defaults-name">
								{t.label ?? t.name}<span class="defaults-id">{t.name}</span>
							</span>
							<div class="segset" role="radiogroup" aria-label="{t.name} 默认">
								{#each DEFAULTS as d (d.value)}
									<button
										class="seg"
										class:active={cur === d.value}
										class:deny={d.value === 'deny' && cur === d.value}
										class:ask={d.value === 'ask' && cur === d.value}
										role="radio"
										aria-checked={cur === d.value}
										onclick={() => setDefault(t.name, d.value)}
									>
										{d.label}
									</button>
								{/each}
							</div>
						</div>
					{/each}
				</div>
			{/if}
		</div>
	{/if}

	{#if listRows.length === 0}
		<p class="empty">{emptyHint}</p>
	{:else}
		<div class="rows">
			{#each listRows as row (row)}
				<div class="row" class:open={row.open}>
					<div class="row-head">
						<button class="row-toggle" onclick={() => toggleOpen(row)} aria-expanded={row.open}>
							<span
								class="verdict"
								class:deny={row.list === 'deny'}
								class:ask={row.list === 'ask'}
								class:allow={row.list === 'allow'}
							>
								{row.list === 'deny' ? '拒绝' : row.list === 'allow' ? '允许' : '询问'}
							</span>
							<span class="row-summary">{summaryOf(row, tools)}</span>
						</button>
						<button class="row-rm" aria-label="删除规则" onclick={() => removeRow(row)}>×</button>
					</div>

					{#if row.open}
						<div class="row-body">
							<div class="ctl-line">
								<div class="segset" role="radiogroup" aria-label="决策">
									{#each DEFAULTS as d (d.value)}
										<button
											class="seg"
											class:active={row.list === d.value}
											class:deny={d.value === 'deny' && row.list === d.value}
											class:ask={d.value === 'ask' && row.list === d.value}
											class:allow={d.value === 'allow' && row.list === d.value}
											role="radio"
											aria-checked={row.list === d.value}
											onclick={() => {
												row.list = d.value as Decision;
												commit();
											}}
										>
											{d.label}
										</button>
									{/each}
								</div>
								<select
									class="in sel"
									value={row.tool}
									onchange={(e) => onToolChange(row, e.currentTarget.value)}
								>
									{#each tools as t (t.name)}
										<option value={t.name}>{t.label ?? t.name}（{t.name}）</option>
									{/each}
									{#if row.tool !== '*' && !tools.some((t) => t.name === row.tool)}
										<option value={row.tool}>{row.tool}</option>
									{/if}
									<option value="*">任意工具（*）</option>
								</select>
							</div>

							{#if row.cond}
								<div class="cond">
									<div class="ctl-line">
										<span class="when">当</span>
										{#if fieldsOf(row.tool).length > 0}
											<select
												class="in sel"
												value={row.field ?? ''}
												onchange={(e) => onFieldChange(row, e.currentTarget.value)}
											>
												{#each fieldsOf(row.tool) as f (f.key)}
													<option value={f.key}>{f.label}</option>
												{/each}
												<option value="">任意字段</option>
											</select>
										{:else}
											<span class="when">输入</span>
										{/if}
										<select
											class="in sel"
											value={row.mode}
											onchange={(e) => {
												row.mode = e.currentTarget.value as MatchMode;
												commit();
											}}
										>
											<option value="substring">包含</option>
											<option value="prefix">以…开头</option>
										</select>
										<label class="negate-toggle">
											<input
												type="checkbox"
												checked={row.negate}
												onchange={(e) => {
													row.negate = e.currentTarget.checked;
													commit();
												}}
											/>
											白名单（仅允许列出的值）
										</label>
									</div>
									<textarea
										class="in ta"
										rows="2"
										value={row.values.join('\n')}
										oninput={(e) => {
											row.values = linesToValues(e.currentTarget.value);
											commit();
										}}
										placeholder={row.mode === 'prefix' ? 'src/\ntmp/' : 'rm -rf\nsudo'}
										spellcheck="false"
									></textarea>
									<button class="btn-ghost sm" onclick={() => closeCondition(row)}>移除条件</button>
								</div>
							{:else}
								<button class="btn-ghost sm add-cond" onclick={() => openCondition(row)}>
									+ 添加条件（默认对整个工具生效）
								</button>
							{/if}
						</div>
					{/if}
				</div>
			{/each}
		</div>
	{/if}

	<button class="btn-ghost sm add" onclick={addRow}>+ 添加规则</button>

	{#if advanced.length > 0}
		<div class="advanced">
			<span class="key">高级规则（{advanced.length}）</span>
			<p class="hint">
				以下规则用本界面无法表达的形式手写，已原样保留、不会被改动。如需编辑请直接改配置文件。
			</p>
			{#each advanced as a, i (i)}
				<code class="adv-rule">[{a.list}] {JSON.stringify(a.rule)}</code>
			{/each}
		</div>
	{/if}
</div>

<style>
	.rules-editor {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.empty {
		font-size: 12px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		padding: var(--space-2) 0;
	}

	/* ---- Rule rows ---- */
	.rows {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.row {
		border: 1px solid transparent;
		border-radius: var(--radius-sm);
	}

	.row.open {
		border-color: var(--border-subtle);
		background: var(--canvas-base);
	}

	.row-head {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		width: 100%;
		padding-right: var(--space-1);
		border-radius: var(--radius-sm);
	}

	.row-toggle {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 6px var(--space-2);
		border-radius: var(--radius-sm);
		text-align: left;
	}

	.row-toggle:hover {
		background: var(--surface-hover);
	}

	.row.open .row-toggle:hover {
		background: transparent;
	}

	.verdict {
		flex-shrink: 0;
		font-family: var(--font-chinese);
		font-size: 11px;
		font-weight: 590;
		padding: 1px 6px;
		border-radius: var(--radius-sm);
	}

	.verdict.deny {
		color: var(--state-error-text);
		background: var(--state-error-bg);
	}

	.verdict.allow {
		color: var(--state-done-text);
		background: var(--state-done-bg);
	}

	.verdict.ask {
		color: var(--state-running-text);
		background: var(--state-running-bg);
	}

	.row-summary {
		flex: 1;
		min-width: 0;
		font-family: var(--font-chinese);
		font-size: 12px;
		color: var(--text-secondary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.row.open .row-summary {
		color: var(--text-primary);
	}

	.row-rm {
		flex-shrink: 0;
		color: var(--text-tertiary);
		font-size: 13px;
		padding: 0 4px;
		border-radius: var(--radius-sm);
		cursor: pointer;
	}

	.row-rm:hover {
		color: var(--state-error-text);
	}

	.row-body {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		padding: 0 var(--space-2) var(--space-2);
	}

	.ctl-line {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex-wrap: wrap;
	}

	.cond {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		align-items: flex-start;
	}

	.cond .ta {
		width: 100%;
	}

	.when {
		font-size: 11px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.negate-toggle {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		font-size: 11px;
		color: var(--text-secondary);
		font-family: var(--font-chinese);
		cursor: pointer;
		white-space: nowrap;
	}

	.add-cond {
		align-self: flex-start;
		color: var(--text-tertiary);
		border-style: dashed;
	}

	.add {
		align-self: flex-start;
	}

	/* ---- Tool defaults table ---- */
	.defaults {
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-base);
	}

	.defaults-head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-3);
		font-family: var(--font-chinese);
		font-size: 12px;
		font-weight: 590;
		color: var(--text-primary);
		text-align: left;
	}

	.chev {
		width: 12px;
		height: 12px;
		color: var(--text-tertiary);
		transition: transform var(--dur-fast) var(--ease-out);
	}

	.chev.open {
		transform: rotate(90deg);
	}

	.defaults-count {
		margin-left: auto;
		font-weight: 400;
		font-size: 11px;
		color: var(--text-tertiary);
	}

	.defaults-body {
		border-top: 1px solid var(--border-subtle);
		padding: var(--space-2) var(--space-3);
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.defaults-row {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: 3px 0;
	}

	.defaults-name {
		flex: 1;
		min-width: 0;
		font-family: var(--font-chinese);
		font-size: 12px;
		color: var(--text-primary);
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
	}

	.defaults-id {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-tertiary);
	}

	/* ---- Segmented control (shared by rows + defaults) ---- */
	.segset {
		display: inline-flex;
		background: var(--canvas-raised);
		border-radius: var(--radius-sm);
		padding: 2px;
		gap: 2px;
	}

	.seg {
		padding: 2px 10px;
		border: none;
		background: transparent;
		color: var(--text-secondary);
		font-size: 12px;
		font-family: var(--font-chinese);
		border-radius: var(--radius-sm);
		cursor: pointer;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out);
	}

	.seg:hover {
		color: var(--text-primary);
	}

	.seg.active {
		background: var(--canvas-float);
		color: var(--text-primary);
	}

	.seg.active.deny {
		color: var(--state-error-text);
	}

	.seg.active.allow {
		color: var(--state-done-text);
	}

	.seg.active.ask {
		color: var(--state-running-text);
	}

	/* ---- Inputs / shared ---- */
	.in {
		padding: 5px 8px;
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

	.sel {
		font-family: var(--font-chinese);
		background: var(--canvas-raised);
	}

	.ta {
		resize: vertical;
		line-height: 1.5;
	}

	.btn-ghost {
		padding: 5px var(--space-3);
		background: transparent;
		color: var(--text-secondary);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		font-size: 12px;
		font-family: var(--font-chinese);
		cursor: pointer;
		transition: all var(--dur-fast) var(--ease-out);
	}

	.btn-ghost:hover {
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.btn-ghost.sm {
		padding: 3px 9px;
	}

	/* ---- Advanced passthrough ---- */
	.advanced {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		border: 1px dashed var(--border-default);
		border-radius: var(--radius-md);
		padding: var(--space-3);
	}

	.key {
		font-family: var(--font-mono);
		font-size: 11px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.hint {
		font-size: 11px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		line-height: 1.5;
	}

	.adv-rule {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-secondary);
		word-break: break-all;
	}
</style>
