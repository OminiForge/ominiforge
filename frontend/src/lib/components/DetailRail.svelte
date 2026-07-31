<script lang="ts">
	import type { SessionMeta } from '$lib/types/SessionMeta';
	import type { RuntimeInfo } from '$lib/types/RuntimeInfo';
	import type { SessionSummary } from '$lib/types/SessionSummary';
	import { num, statLabel, formatCost, cacheLabel, topTools } from '$lib/stats';
	import { inspectDetail, type InspectRow, type RawEvent } from '$lib/inspect';

	/** The right detail rail: Info (config-layer session context + live context
	 *  occupancy + folded stats) and Inspect (raw event timeline) as switchable
	 *  tabs. The rail never opens/closes itself — that's the topbar toggle's
	 *  job; tab switching only flips which pane shows. */
	let {
		inspectMode,
		inspectReversed = $bindable(),
		inspectTick,
		inspectRows,
		meta,
		runtime,
		divergent,
		context,
		summary,
		onSetInspect,
		onScrollToSeq
	}: {
		inspectMode: boolean;
		inspectReversed: boolean;
		/** Bumped once per batched raw-log append; read here so the timeline
		 *  re-derives (rawLog itself is intentionally NOT reactive). */
		inspectTick: number;
		inspectRows: InspectRow[];
		meta: SessionMeta | null;
		runtime: RuntimeInfo | null;
		/** Runtime-layer models that diverge from the configured one (B4). */
		divergent: string[];
		context: { tokens: number; window: number; threshold: number } | null;
		summary: SessionSummary | null;
		onSetInspect: (on: boolean) => void;
		/** Jump the conversation to the item at/around an event's seq. */
		onScrollToSeq: (seq: number) => void;
	} = $props();

	/** Expanded inspect groups, keyed by group key. */
	let inspectExpanded = $state<Record<string, boolean>>({});

	/** Short workspace label for the INFO panel: last two path segments, full
	 *  path on hover. */
	function wsLabel(ws: string): string {
		const parts = ws.split('/').filter(Boolean);
		return parts.length > 2 ? '…/' + parts.slice(-2).join('/') : ws;
	}
</script>

<aside class="detail">
	<!-- Rail tabs: Info (config + stats) and Inspect (raw event timeline)
	     share the rail; the topbar clock toggle selects the inspect tab. -->
	<div class="detail-tabs" role="tablist">
		<button
			class="detail-tab"
			class:on={!inspectMode}
			role="tab"
			aria-selected={!inspectMode}
			onclick={() => onSetInspect(false)}
		>
			Info
		</button>
		<button
			class="detail-tab"
			class:on={inspectMode}
			role="tab"
			aria-selected={inspectMode}
			onclick={() => onSetInspect(true)}
		>
			Inspect
		</button>
		{#if inspectMode}
			<!-- Timeline order toggle: newest-first reads like a log tail. -->
			<button
				class="inspect-order"
				onclick={() => (inspectReversed = !inspectReversed)}
				title={inspectReversed
					? 'Newest first, click for chronological'
					: 'Chronological, click for newest first'}
				aria-label="Toggle timeline order"
				aria-pressed={inspectReversed}
			>
				{#if inspectReversed}↓ Newest{:else}↑ Oldest{/if}
			</button>
		{/if}
	</div>

	{#if !inspectMode}
		<div class="detail-info">
			<!-- INFO: config-layer session context (moved off the global sidebar) -->
			<section class="detail-section">
				<div class="detail-label">Info</div>
				{#if meta?.workspace}
					<div class="kv">
						<div class="kv-key">Workspace</div>
						<div class="kv-val" title={meta.workspace}>{wsLabel(meta.workspace)}</div>
					</div>
				{/if}
				{#if runtime && runtime.env.length > 0}
					<div class="kv">
						<div class="kv-key">Env</div>
						<div class="kv-val" title={runtime.env.join(' · ')}>{runtime.env.join(' · ')}</div>
					</div>
				{/if}
				{#if runtime}
					<div class="kv">
						<div class="kv-key">Model</div>
						<div class="kv-val" title={`${runtime.provider} · ${runtime.model}`}>
							{runtime.model}
						</div>
					</div>
				{/if}
				{#if divergent.length > 0}
					<div class="kv warn">
						<div class="kv-key warn-key">⚠ Runtime</div>
						<div
							class="kv-val warn-val"
							title={`runtime used ${divergent.join(', ')}, configured ${runtime?.model}`}
						>
							{divergent.join(' · ')} ≠ {runtime?.model}
						</div>
					</div>
				{/if}
				{#if meta?.profile_id}
					<div class="kv">
						<div class="kv-key">Profile</div>
						<div class="kv-val">{meta.profile_id}</div>
					</div>
				{/if}
			</section>

			<!-- CONTEXT: live per-round window occupancy (context_updated event). Its
     own section (driven by live events, not the summary endpoint) so it
     shows mid-turn even before the first summary snapshot loads. -->
			{#if context}
				{@const pct = context.window
					? Math.min(100, (context.tokens / context.window) * 100)
					: null}
				{@const overThreshold = pct !== null && pct / 100 >= context.threshold}
				<section class="detail-section">
					<div class="detail-label">Context</div>
					<div class="ctx">
						<div class="ctx-nums">
							<span class="ctx-val">{context.tokens.toLocaleString()}</span>
							{#if context.window}
								<span class="ctx-limit">/ {context.window.toLocaleString()}</span>
							{/if}
						</div>
						{#if pct !== null}
							<div
								class="ctx-track"
								title={`${context.tokens.toLocaleString()} / ${context.window.toLocaleString()} tokens · compaction at ${(context.threshold * 100).toFixed(0)}%`}
							>
								<span class="ctx-fill" class:warn={overThreshold} style="width: {pct}%"></span>
								<!-- compaction-threshold tick -->
								<span class="ctx-tick" style="left: {context.threshold * 100}%"></span>
							</div>
							<span class="ctx-pct" class:warn={overThreshold}>{pct.toFixed(0)}%</span>
						{:else}
							<span class="ctx-pct unpriced">window unknown</span>
						{/if}
					</div>
				</section>
			{/if}

			<!-- STATS: folded summary snapshot, refreshed on each settled turn -->
			{#if summary}
				{@const s = summary}
				{@const tools = topTools(s, 6)}
				<section class="detail-section">
					<div class="detail-label">Stats</div>
					<div class="stat-grid">
						<div class="stat">
							<span class="stat-value">{s.total_turns}</span>
							<span class="stat-key">{statLabel.turns(s.total_turns)}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{s.total_model_requests}</span>
							<span class="stat-key">{statLabel.reqs(s.total_model_requests)}</span>
						</div>
						<div class="stat">
							<span class="stat-value">
								{s.total_tool_calls}{#if s.total_tool_failures > 0}<span class="stat-fail"
										>/{s.total_tool_failures}✗</span
									>{/if}
							</span>
							<span class="stat-key">{statLabel.toolCalls(s.total_tool_calls)}</span>
						</div>
						<div class="stat">
							<span class="stat-value cost" class:unpriced={s.cost_usd == null}
								>{formatCost(s)}</span
							>
							<span class="stat-key">{statLabel.cost}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{num(s.total_input_tokens).toLocaleString()}</span>
							<span class="stat-key">{statLabel.inTok}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{num(s.total_output_tokens).toLocaleString()}</span>
							<span class="stat-key">{statLabel.outTok}</span>
						</div>
						<div class="stat">
							<span class="stat-value">{cacheLabel(s)}</span>
							<span class="stat-key">{statLabel.cache}</span>
						</div>
					</div>
				</section>

				{#if tools.length > 0}
					<section class="detail-section">
						<div class="detail-label">Tool usage</div>
						<ul class="bars">
							{#each tools as t (t.tool)}
								<li class="bar-row">
									<span class="bar-label" title={t.tool}>{t.tool}</span>
									<span class="bar-track"
										><span class="bar-fill" style="width: {t.pct}%"></span></span
									>
									<span class="bar-count">{t.count}</span>
								</li>
							{/each}
						</ul>
					</section>
				{/if}
			{/if}
		</div>
	{:else}
		<!-- INSPECT: raw event timeline. Read rawLog imperatively; inspectTick
	     re-triggers the read after each batched burst (rawLog itself is
	     intentionally NOT reactive — see its declaration). -->
		{@const _ = inspectTick}
		<div class="inspect-timeline">
			{#each inspectRows as row (row.type === 'group' ? row.group.key : row.ev.seq)}
				{#if row.type === 'single'}
					{@const info = inspectDetail(row.ev)}
					<button
						class="inspect-event"
						onclick={() => onScrollToSeq(Number(row.ev.seq))}
						title="Jump to the matching position in the conversation"
					>
						<span class="inspect-seq">#{row.ev.seq}</span>
						<span class="inspect-type">{info.variant}</span>
						{#if info.detail}<span class="inspect-detail">{info.detail}</span>{/if}
						<span class="inspect-time">{new Date(row.ev.timestamp).toLocaleTimeString()}</span>
					</button>
				{:else}
					{@const g = row.group}
					<!-- Group row: click expands/collapses the phase list; the ⧉
					     button jumps to the group's conversation position. -->
					<div class="inspect-group">
						<button
							class="inspect-event inspect-group-head"
							onclick={() =>
								(inspectExpanded = { ...inspectExpanded, [g.key]: !inspectExpanded[g.key] })}
							aria-expanded={!!inspectExpanded[g.key]}
							title="Expand/collapse phases"
						>
							<span class="inspect-seq">#{g.seq}</span>
							<span class="inspect-caret" class:open={!!inspectExpanded[g.key]}>▸</span>
							<span class="inspect-type">{g.label}</span>
							{#if g.detail}<span class="inspect-detail">{g.detail}</span>{/if}
							<span class="inspect-count">{g.events.length}</span>
						</button>
						<button
							class="inspect-jump"
							onclick={() => onScrollToSeq(g.seq)}
							title="Jump to the matching position in the conversation"
							aria-label="Jump to the matching position in the conversation"
						>
							⧉
						</button>
						{#if inspectExpanded[g.key]}
							{#each g.events as ev (ev.seq)}
								{@const info = inspectDetail(ev)}
								<button
									class="inspect-event inspect-phase"
									onclick={() => onScrollToSeq(Number(ev.seq))}
									title="Jump to the matching position in the conversation"
								>
									<span class="inspect-seq">#{ev.seq}</span>
									<span class="inspect-type">{info.variant}</span>
									{#if info.detail}<span class="inspect-detail">{info.detail}</span>{/if}
									<span class="inspect-time">{new Date(ev.timestamp).toLocaleTimeString()}</span>
								</button>
							{/each}
						{/if}
					</div>
				{/if}
			{/each}
		</div>
	{/if}
</aside>

<style>
	/* ---- DETAIL RAIL (INFO + STATS + tool usage) ---- */
	.detail {
		height: 100%;
		overflow-y: auto;
		border-left: 1px solid var(--border-subtle);
		background: var(--canvas-raised);
		padding: var(--space-5) var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-5);
	}

	.detail-section {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.detail-label {
		font-size: 10.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.07em;
		text-transform: uppercase;
	}

	.kv {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.kv-key {
		font-family: var(--font-mono);
		font-size: 9.5px;
		font-weight: 510;
		color: var(--text-tertiary);
		letter-spacing: 0.09em;
		text-transform: uppercase;
		line-height: 1;
	}

	.kv-val {
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
	.warn-key {
		color: var(--state-error-text);
	}
	.warn-val {
		color: var(--state-error-text);
		white-space: normal;
	}

	.stat-grid {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: var(--space-3) var(--space-4);
	}

	.stat {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}

	.stat-value {
		font-family: var(--font-mono);
		font-size: 14px;
		font-weight: 500;
		color: var(--text-primary);
		font-variant-numeric: tabular-nums;
		line-height: 1.2;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.stat-value.cost {
		color: var(--accent-ink);
	}
	.stat-value.cost.unpriced {
		color: var(--text-tertiary);
		font-size: 12px;
	}

	.stat-fail {
		color: var(--state-error-text);
		font-size: 11px;
	}

	.stat-key {
		font-size: 9.5px;
		font-weight: 510;
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--text-tertiary);
	}

	.bars {
		list-style: none;
		display: grid;
		gap: 6px;
	}

	.bar-row {
		display: grid;
		grid-template-columns: minmax(48px, 84px) 1fr auto;
		align-items: center;
		gap: var(--space-2);
	}

	.bar-label {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-secondary);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.bar-track {
		background: var(--canvas-float);
		border-radius: var(--radius-sm);
		height: 6px;
		overflow: hidden;
	}

	.bar-fill {
		display: block;
		height: 100%;
		/* Muted, not accent: dense data, not the screen's one CTA. */
		background: var(--text-tertiary);
		border-radius: var(--radius-sm);
		transition: width var(--dur-std) var(--ease-out);
	}

	.bar-count {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
		min-width: 2ch;
		text-align: right;
	}

	/* ---- CONTEXT (live per-round window occupancy) ---- */
	.ctx {
		display: grid;
		grid-template-columns: 1fr auto;
		align-items: center;
		gap: var(--space-2) var(--space-3);
	}
	.ctx-nums {
		font-family: var(--font-mono);
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
	}
	.ctx-val {
		font-size: 14px;
		font-weight: 500;
		color: var(--text-primary);
	}
	.ctx-limit {
		font-size: 12px;
		color: var(--text-tertiary);
	}
	.ctx-track {
		grid-column: 1 / -1;
		order: 3;
		position: relative;
		background: var(--canvas-float);
		border-radius: var(--radius-sm);
		height: 6px;
		overflow: hidden;
	}
	.ctx-fill {
		display: block;
		height: 100%;
		background: var(--text-tertiary);
		border-radius: var(--radius-sm);
		transition: width var(--dur-std) var(--ease-out);
	}
	.ctx-fill.warn {
		background: var(--state-error-text);
	}
	/* Compaction-threshold marker. */
	.ctx-tick {
		position: absolute;
		top: 0;
		bottom: 0;
		width: 1px;
		background: var(--text-secondary);
		transform: translateX(-0.5px);
	}
	.ctx-pct {
		font-family: var(--font-mono);
		font-size: 12px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
		text-align: right;
	}
	.ctx-pct.warn {
		color: var(--state-error-text);
	}
	.ctx-pct.unpriced {
		font-size: 11px;
	}

	/* ---- RAIL TABS + INSPECT (raw event timeline) ---- */
	/* Info and Inspect share the right rail as switchable tabs; the inspect
	 *  timeline lives inside the rail instead of floating over the viewport. */
	/* The rail scrolls as a whole (info sections and the inspect timeline
	 *  alike); the tab bar sticks to the top so the switch is always reachable. */
	.detail-tabs {
		display: flex;
		gap: var(--space-1);
		border-bottom: 1px solid var(--border-subtle);
		padding-bottom: var(--space-2);
		position: sticky;
		top: calc(-1 * var(--space-5));
		background: var(--canvas-raised);
		padding-top: var(--space-1);
		z-index: 1;
	}

	.detail-tab {
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		padding: var(--space-1) var(--space-2);
		font-family: var(--font-mono);
		font-size: 11px;
		font-weight: 510;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--text-tertiary);
		cursor: pointer;
	}

	.detail-tab:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	.detail-tab.on {
		color: var(--text-secondary);
		background: var(--canvas-overlay);
	}

	/* Inspect order toggle: pinned to the tab bar's right edge, shown only
	 *  while the inspect tab is active. */
	.inspect-order {
		margin-left: auto;
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		padding: var(--space-1) var(--space-2);
		font-family: var(--font-mono);
		font-size: 10.5px;
		color: var(--text-tertiary);
		cursor: pointer;
		white-space: nowrap;
	}

	.inspect-order:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	/* The info tab scrolls its sections together with the rail. */
	.detail-info {
		display: contents;
	}

	.inspect-timeline {
		padding: 0;
	}

	.inspect-event {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-2) var(--space-3);
		border: none;
		border-radius: var(--radius-sm);
		font-family: var(--font-mono);
		font-size: 11px;
		border-bottom: 1px solid var(--border-subtle);
		/* Now a <button> (click jumps to the conversation position): reset the
		   UA button chrome so it still reads as a log row. */
		width: 100%;
		background: transparent;
		color: inherit;
		text-align: left;
		cursor: pointer;
	}

	.inspect-event:hover {
		background: var(--canvas-raised);
	}

	.inspect-seq {
		color: var(--text-tertiary);
		min-width: 3ch;
		text-align: right;
	}

	.inspect-type {
		color: var(--accent-ink);
		font-weight: 500;
		white-space: nowrap;
	}

	.inspect-detail {
		color: var(--text-secondary);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}

	.inspect-time {
		color: var(--text-tertiary);
		margin-left: auto;
		font-variant-numeric: tabular-nums;
	}

	/* Group rows: the head expands/collapses phases; the ⧉ jump button sits at
	 *  the row's right edge (the head's count badge replaces the time there). */
	.inspect-group {
		position: relative;
	}

	.inspect-caret {
		color: var(--text-tertiary);
		transition: transform var(--dur-fast) var(--ease-out);
	}

	.inspect-caret.open {
		transform: rotate(90deg);
	}

	.inspect-count {
		margin-left: auto;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
		padding-right: 20px; /* room for the absolutely-positioned jump button */
	}

	.inspect-jump {
		position: absolute;
		top: 50%;
		right: var(--space-2);
		transform: translateY(-50%);
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--text-tertiary);
		font-size: 11px;
		cursor: pointer;
		padding: 2px 4px;
	}

	.inspect-jump:hover {
		color: var(--text-primary);
		background: var(--surface-hover);
	}

	/* Phase rows sit inside an expanded group: indented, quieter, and without
	 *  the row separator (the group's own bottom border closes the block). */
	.inspect-phase {
		padding-left: var(--space-6);
		border-bottom: none;
		color: var(--text-tertiary);
	}

	.inspect-phase .inspect-type {
		color: var(--text-secondary);
		font-weight: 400;
	}

	/* Narrow: stack the rail under the conversation instead of beside it. */
	@media (max-width: 900px) {
		.detail {
			height: auto;
			border-left: none;
			border-top: 1px solid var(--border-subtle);
		}
	}
</style>
