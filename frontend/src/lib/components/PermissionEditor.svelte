<script lang="ts">
	import type { PermissionPolicy } from '$lib/types/PermissionPolicy';
	import type { ToolInfo } from '$lib/types/ToolInfo';
	import type { MatchMode } from '$lib/types/MatchMode';
	import {
		toCards,
		fromCards,
		type ToolCard,
		type Exception,
		type Decision
	} from '$lib/permission-cards';

	/** Card-based editor for a {@link PermissionPolicy} at any tier (profile /
	 *  gateway / workspace, `doc/permission.md` §3.2). The user never types a tool
	 *  name or a JSON field: each tool from the catalog is a card with a three-way
	 *  default and per-tool exception controls. The card ⇄ policy mapping lives in
	 *  the tested pure module `permission-cards.ts`; this component only renders. */
	let {
		policy = $bindable(),
		tools = []
	}: { policy: PermissionPolicy; tools: ToolInfo[] } = $props();

	// Compile the incoming policy into cards once per (policy identity, catalog).
	// We keep the card model as the editable state and push a recompiled policy
	// back on every change, so the parent's `policy` stays the source of truth on
	// save without us re-deriving cards on our own writes (which would fight the
	// user's focus). `advanced` (rules the cards can't express) rides along.
	const model = $derived(toCards(policy, tools));
	let cards = $state<ToolCard[]>([]);
	let advanced = $state<{ list: 'deny' | 'ask'; rule: import('$lib/types/Rule').Rule }[]>([]);

	// Seed local state whenever the compiled model changes (policy/tools swap).
	// Guarded by a signature so our own writes (which change `policy`) don't wipe
	// in-progress edits: we only reseed when the *incoming* policy differs from
	// what our cards would produce.
	let seededSig = $state('');
	$effect(() => {
		const sig = JSON.stringify(policy) + '|' + tools.map((t) => t.name).join(',');
		if (sig !== seededSig) {
			cards = model.cards.map((c) => ({ ...c, exceptions: c.exceptions.map((e) => ({ ...e })) }));
			advanced = model.advanced;
			seededSig = sig;
		}
	});

	// Push card edits back to the bound policy. Called after every mutation.
	function commit() {
		const next = fromCards({ cards, advanced });
		policy.deny = next.deny;
		policy.ask = next.ask;
		// Keep our signature in sync so the reseed effect doesn't clobber us.
		seededSig = JSON.stringify(policy) + '|' + tools.map((t) => t.name).join(',');
	}

	const DECISIONS: { value: Decision; label: string }[] = [
		{ value: 'allow', label: '允许' },
		{ value: 'ask', label: '询问' },
		{ value: 'deny', label: '拒绝' }
	];

	function setDefault(card: ToolCard, d: Decision) {
		card.default = d;
		commit();
	}

	// A new exception defaults to the tool's first field (path tools → prefix
	// mode) so the common case needs no dropdown fiddling.
	function addException(card: ToolCard) {
		const field = card.info?.fields?.[0];
		const ex: Exception = {
			decision: 'deny',
			field: field?.key ?? null,
			mode: field?.is_path ? 'prefix' : 'substring',
			negate: false,
			values: []
		};
		card.exceptions = [...card.exceptions, ex];
		commit();
	}

	function removeException(card: ToolCard, i: number) {
		card.exceptions = card.exceptions.filter((_, idx) => idx !== i);
		commit();
	}

	// values <-> textarea: one pattern per non-empty line.
	function valuesToLines(v: string[]): string {
		return v.join('\n');
	}
	function linesToValues(v: string): string[] {
		return v
			.split('\n')
			.map((s) => s.trim())
			.filter((s) => s.length > 0);
	}

	function fieldIsPath(card: ToolCard, key: string | null): boolean {
		return !!card.info?.fields?.find((f) => f.key === key)?.is_path;
	}

	// A one-line plain-language reading of an exception, shown read-only above its
	// controls so a user never has to mentally decode field + mode + negate — the
	// antidote to the "不属于（白名单）" double negative.
	function exceptionSummary(card: ToolCard, ex: Exception): string {
		const verb = ex.decision === 'deny' ? '拒绝' : '询问';
		const fieldLabel = ex.field
			? (card.info?.fields?.find((f) => f.key === ex.field)?.label ?? ex.field)
			: '输入';
		const vals = ex.values.length ? ex.values.join('、') : '（未填）';
		if (ex.negate) {
			// Allow-list: match when NOT any value → the listed values are permitted.
			const rel = ex.mode === 'prefix' ? '位于' : '包含';
			return `${verb}：当 ${fieldLabel} 不${rel} ${vals}（即仅允许这些）`;
		}
		const rel = ex.mode === 'prefix' ? '以…开头' : '包含';
		return `${verb}：当 ${fieldLabel} ${rel} ${vals}`;
	}
</script>

<div class="cards">
	{#each cards as card (card.tool)}
		<div class="card">
			<div class="card-head">
				<div class="card-title">
					<span class="tool-name">{card.info?.label ?? card.tool}</span>
					<span class="tool-id">{card.tool}</span>
				</div>
				{#if card.info?.description}
					<span class="tool-desc">{card.info.description}</span>
				{/if}
			</div>

			<div class="row">
				<span class="key">默认</span>
				<div class="segset" role="radiogroup" aria-label="{card.tool} 默认">
					{#each DECISIONS as d (d.value)}
						<button
							class="seg"
							class:active={card.default === d.value}
							class:deny={d.value === 'deny' && card.default === d.value}
							class:ask={d.value === 'ask' && card.default === d.value}
							role="radio"
							aria-checked={card.default === d.value}
							onclick={() => setDefault(card, d.value)}
						>
							{d.label}
						</button>
					{/each}
				</div>
			</div>

			{#if card.exceptions.length > 0}
				<div class="exceptions">
					{#each card.exceptions as ex, i (i)}
						<div class="exc">
							<p class="exc-summary">{exceptionSummary(card, ex)}</p>
							<div class="exc-controls">
								<select
									class="in sel"
									value={ex.decision}
									onchange={(e) => {
										ex.decision = e.currentTarget.value as 'ask' | 'deny';
										commit();
									}}
								>
									<option value="deny">拒绝</option>
									<option value="ask">询问</option>
								</select>
								{#if card.info?.fields && card.info.fields.length > 0}
									<span class="exc-when">当</span>
									<select
										class="in sel"
										value={ex.field ?? ''}
										onchange={(e) => {
											const key = e.currentTarget.value || null;
											ex.field = key;
											// A path field defaults to prefix; others to substring.
											ex.mode = fieldIsPath(card, key) ? 'prefix' : 'substring';
											commit();
										}}
									>
										{#each card.info.fields as f (f.key)}
											<option value={f.key}>{f.label}</option>
										{/each}
										<option value="">任意字段</option>
									</select>
								{:else}
									<span class="exc-when">当输入</span>
								{/if}
								<select
									class="in sel"
									value={ex.mode}
									onchange={(e) => {
										ex.mode = e.currentTarget.value as MatchMode;
										commit();
									}}
								>
									<option value="substring">包含</option>
									<option value="prefix">以…开头</option>
								</select>
								<label class="negate-toggle">
									<input
										type="checkbox"
										checked={ex.negate}
										onchange={(e) => {
											ex.negate = e.currentTarget.checked;
											commit();
										}}
									/>
									取反（白名单）
								</label>
							</div>
							<div class="exc-values">
								<textarea
									class="in ta"
									rows="2"
									value={valuesToLines(ex.values)}
									oninput={(e) => {
										ex.values = linesToValues(e.currentTarget.value);
										commit();
									}}
									placeholder={ex.mode === 'prefix' ? 'src/\ntmp/' : 'rm -rf\nsudo'}
									spellcheck="false"
								></textarea>
								<button
									class="btn-ghost danger rm"
									onclick={() => removeException(card, i)}
									aria-label="删除例外">×</button
								>
							</div>
						</div>
					{/each}
				</div>
			{/if}

			<button class="btn-ghost sm add" onclick={() => addException(card)}>+ 例外</button>
		</div>
	{/each}

	{#if advanced.length > 0}
		<div class="advanced">
			<span class="key">高级规则（{advanced.length}）</span>
			<p class="hint">
				以下规则用通配或本界面无法表达的形式手写，已原样保留、不会被改动。如需编辑请直接改配置文件。
			</p>
			{#each advanced as a, i (i)}
				<code class="adv-rule">[{a.list}] {JSON.stringify(a.rule)}</code>
			{/each}
		</div>
	{/if}
</div>

<style>
	.cards {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.card {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--canvas-base);
		padding: var(--space-3);
	}

	.card-head {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.card-title {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
	}

	.tool-name {
		font-size: 13px;
		font-weight: 590;
		color: var(--text-primary);
		font-family: var(--font-chinese);
	}

	.tool-id {
		font-family: var(--font-mono);
		font-size: 10px;
		color: var(--text-tertiary);
	}

	.tool-desc {
		font-size: 11px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.row {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.key {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	/* Three-way default segmented control. Active segment is tinted by verdict:
	   deny=error, ask=running, allow=accent — state redundancy (§1.3). */
	.segset {
		display: inline-flex;
		/* Recessed track (a step above the card's --canvas-base) so the active
		   pill can lift ABOVE it — the "bright pill on dark track" convention. */
		background: var(--canvas-raised);
		border-radius: var(--radius-sm);
		padding: 2px;
		gap: 2px;
	}

	.seg {
		padding: 3px 12px;
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
		/* Lifted pill: brightest surface in the ladder, clearly above the track. */
		background: var(--canvas-float);
		color: var(--text-primary);
	}

	.seg.active.deny {
		color: var(--state-error-text);
	}

	.seg.active.ask {
		color: var(--state-running-text);
	}

	.exceptions {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		border-top: 1px solid var(--border-subtle);
		padding-top: var(--space-2);
	}

	.exc {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}

	.exc-controls {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex-wrap: wrap;
	}

	.exc-when {
		font-size: 11px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}

	.exc-summary {
		margin: 0;
		font-size: 11px;
		line-height: 1.4;
		color: var(--text-secondary);
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

	.exc-values {
		display: flex;
		gap: var(--space-2);
		align-items: flex-start;
	}

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
		flex: 1;
		resize: vertical;
		line-height: 1.5;
		width: 100%;
	}

	.rm {
		padding: 2px 7px;
		margin-top: 2px;
	}

	.add {
		align-self: flex-start;
	}

	.advanced {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		border: 1px dashed var(--border-default);
		border-radius: var(--radius-md);
		padding: var(--space-3);
	}

	.hint {
		font-size: 11px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		margin: 0;
		line-height: 1.5;
	}

	.adv-rule {
		font-family: var(--font-mono);
		font-size: 10.5px;
		color: var(--text-secondary);
		word-break: break-all;
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

	.btn-ghost.danger:hover {
		color: var(--state-error-text);
		border-color: var(--state-error-text);
	}

	.btn-ghost.sm {
		padding: 3px 9px;
	}
</style>
