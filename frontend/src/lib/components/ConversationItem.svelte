<script lang="ts">
	import { fly, type FlyParams } from 'svelte/transition';
	import { browser } from '$app/environment';
	import type { Item } from '$lib/conversation';
	import type { ApprovalScope } from '$lib/types/ApprovalScope';
	import { renderMarkdown, renderUserMarkdown } from '$lib/markdown';
	import ToolBlock from '$lib/components/tools/ToolBlock.svelte';
	import TodoCard from '$lib/components/TodoCard.svelte';

	/** One conversation item (one iteration of the stream's #each): user bubble
	 *  with turn actions, assistant text, collapsible reasoning, tool card, todo
	 *  card, one-line activity row, or an error/notice. Collapse state lives in
	 *  the parent's per-index map (`collapsed` + `onToggleCollapse`) so it
	 *  survives streaming updates that replace the item object. */
	let {
		item,
		index,
		enterAnim,
		isDraft,
		firstUserSeq,
		dockTodoIndex,
		collapsed,
		onFork,
		onToggleCollapse,
		onDecide
	}: {
		item: Item;
		/** Position in the items array — the collapse-map key and DOM anchor. */
		index: number;
		/** Entry transition (history items get a no-op one from the caller). */
		enterAnim: FlyParams;
		isDraft: boolean;
		/** Seq of the first user message: it gets no fork affordance (branching
		 *  before it inherits an empty context = a plain new session). */
		firstUserSeq: number | null;
		/** Index of the todo card shown in the sticky dock, skipped inline. */
		dockTodoIndex: number | undefined;
		/** Explicit per-index collapse overrides; absent = the item's default. */
		collapsed: Record<number, boolean>;
		onFork: (msgSeq: number, text: string) => void;
		onToggleCollapse: (item: Item, i: number) => void;
		onDecide: (
			callId: string,
			decision: 'approve' | 'reject',
			scope: ApprovalScope
		) => void | Promise<void>;
	} = $props();

	function isCollapsed(item: Item, i: number): boolean {
		if (i in collapsed) return collapsed[i];
		// Reasoning is collapsed by default in every state: streaming shows a
		// one-line ticker of the latest line, finished shows a first-line
		// preview. The user expands on demand (tools stay open per preference).
		if (item.kind === 'reasoning') return true;
		// Auto-collapse a todo list once every step is terminal (the work is done);
		// keep an active todo list open so the running task stays visible.
		if (item.kind === 'todo') {
			return (
				item.steps.length > 0 &&
				item.steps.every(
					(s) => s.status === 'completed' || s.status === 'cancelled' || s.status === 'blocked'
				)
			);
		}
		return false;
	}

	function shortPreview(text: string): string {
		const first = text.split('\n')[0].slice(0, 60);
		return first.length < text.split('\n')[0].length ? first + '…' : first;
	}
	/** Latest non-empty line of a streaming reasoning block — the collapsed
	 *  ticker line the user watches roll by while the model thinks. */
	function lastLine(text: string): string {
		const lines = text.split('\n');
		for (let i = lines.length - 1; i >= 0; i--) {
			const line = lines[i].trim();
			if (line) return line;
		}
		return '';
	}
</script>

{#if item.kind === 'user'}
	<div
		class="item item-user"
		class:pending={item.pending}
		data-user-anchor={item.pending ? undefined : index}
		data-item-anchor={index}
		in:fly|local={enterAnim}
	>
		{#if item.seq != null && !isDraft && item.seq !== firstUserSeq}
			<!-- Turn actions: low-frequency per-turn operations, anchored to
		     the user message that starts the turn. Faint by default
		     (--text-disabled), brightens when the turn is hovered. Icon +
		     tooltip only — no visible label (the glyph carries the meaning).
		     Fork is the only action today; the row is a flex container so a
		     future rating control slots in beside it. See DESIGN.md §4.7.
		     Excluded on the first user turn: branching before it inherits an
		     empty context, i.e. just a new session. -->
			<div class="turn-actions">
				<button
					class="turn-action-btn"
					onclick={() => onFork(item.seq!, item.text)}
					title="Fork from this message (inherits the prior conversation into a new session)"
					aria-label="Fork a new session from this message"
				>
					<!-- Original divergence glyph: a single stem splitting into
				     two branches — drawn from scratch, not a borrowed
				     git-branch icon (DESIGN §5). Round caps read as soft
				     nodes at the three tips. -->
					<svg
						class="turn-action-icon"
						viewBox="0 0 14 14"
						fill="none"
						stroke="currentColor"
						stroke-width="1.5"
						stroke-linecap="round"
						stroke-linejoin="round"
						aria-hidden="true"
					>
						<path d="M7 12V7.2" />
						<path d="M7 7.2 3.8 4" />
						<path d="M7 7.2 10.2 4" />
					</svg>
				</button>
			</div>
		{/if}
		<div class="user-bubble item-text">
			{#if browser}
				<!-- eslint-disable-next-line svelte/no-at-html-tags -->
				{@html renderUserMarkdown(item.text)}
			{:else}
				{item.text}
			{/if}
		</div>
		{#if item.pending}
			<!-- Optimistic echo awaiting its committed Turn::Started.
			     A healthy stream replaces it in a blink; this hint only
			     lingers when the stream is dead (reconnect underway). -->
			<span class="pending-hint">Sent · syncing…</span>
		{/if}
	</div>
{:else if item.kind === 'text'}
	{#if item.text.trim()}
		<div
			class="item item-text"
			class:streaming={item.streaming}
			data-item-anchor={index}
			in:fly|local={enterAnim}
		>
			{#if browser}
				<!-- eslint-disable-next-line svelte/no-at-html-tags -->
				{@html renderMarkdown(item.text)}
			{:else}
				{item.text}
			{/if}
		</div>
	{/if}
{:else if item.kind === 'reasoning'}
	{#if item.text.trim()}
		<div
			class="item item-reasoning"
			class:streaming={item.streaming}
			class:expanded={!isCollapsed(item, index)}
			data-item-anchor={index}
			in:fly|local={enterAnim}
		>
			{#if item.streaming}
				<!-- Streaming: collapsed by default — a one-line ticker
				     showing the latest reasoning line (the roll of lines
				     is the liveness cue). Click to expand the full stream;
				     click again to fold back. -->
				<button
					class="reasoning-preview"
					onclick={() => onToggleCollapse(item, index)}
					aria-expanded={!isCollapsed(item, index)}
				>
					{#if isCollapsed(item, index)}
						<span class="reasoning-inline-label">Thinking</span>
						<span class="streaming-dot"></span>
						<span class="reasoning-ticker">{lastLine(item.text)}</span>
					{:else}
						<span class="reasoning-inline-label">Thinking</span>
						<span class="streaming-dot"></span>
						Collapse
					{/if}
				</button>
				{#if !isCollapsed(item, index)}
					<div class="reasoning-stream">
						{#if browser}
							<!-- eslint-disable-next-line svelte/no-at-html-tags -->
							{@html renderMarkdown(item.text)}
						{:else}
							{item.text}
						{/if}
					</div>
				{/if}
			{:else}
				<!-- Done: single-line muted preview, click to expand. -->
				<button
					class="reasoning-preview"
					onclick={() => onToggleCollapse(item, index)}
					aria-expanded={!isCollapsed(item, index)}
				>
					{#if isCollapsed(item, index)}
						{shortPreview(item.text)}
					{:else}
						Collapse thinking
					{/if}
				</button>
				{#if !isCollapsed(item, index)}
					<div class="reasoning-body">
						{#if browser}
							<!-- eslint-disable-next-line svelte/no-at-html-tags -->
							{@html renderMarkdown(item.text)}
						{:else}
							{item.text}
						{/if}
					</div>
				{/if}
			{/if}
		</div>
	{/if}
{:else if item.kind === 'tool'}
	<div class="item" data-item-anchor={index} in:fly|local={enterAnim}>
		<ToolBlock {item} {onDecide} />
	</div>
{:else if item.kind === 'todo'}
	<!-- Streaming placeholders (item.streaming) render nothing: a flashing
     inline "planning…" card on every todo op is pure eye-strain, and
     the dock already shows the live list. The docked card is shown in
     the dock, so skip it here too — only committed history cards render. -->
	{#if !item.streaming && index !== dockTodoIndex}
		<div class="item" data-item-anchor={index} in:fly|local={enterAnim}>
			<TodoCard
				steps={item.steps}
				expanded={!isCollapsed(item, index)}
				onToggle={() => onToggleCollapse(item, index)}
			/>
		</div>
	{/if}
{:else if item.kind === 'activity'}
	<!-- One-line operation trace (todo op / hook execution / runtime
	     reminder): a quiet left-aligned rail row, lighter than a tool
	     card but visible on the timeline unlike the inspect panel.
	     detail carries the why (hook block reason, reminder text) as
	     a chip — hook labels are fixed-shape (name @ point → outcome),
	     so the variable-length reason must not trail raw text. A
	     multi-line detail (a runtime reminder) is truncated on the
	     row and expandable in full below it. -->
	{@const detailInline =
		item.detail && !item.detail.includes('\n')
			? item.detail.replace(/<\/?reminder>/g, '').trim()
			: undefined}
	{@const detailBlock =
		item.detail && item.detail.includes('\n')
			? item.detail.replace(/<\/?reminder>/g, '').trim()
			: undefined}
	<div
		class="item item-activity"
		class:activity-blocked={item.icon === 'hook' && !!item.detail}
		in:fly|local={enterAnim}
	>
		<span class="activity-mark" aria-hidden="true">
			{#if item.icon === 'hook'}
				<svg
					viewBox="0 0 12 12"
					fill="none"
					stroke="currentColor"
					stroke-width="1.4"
					stroke-linecap="round"
					stroke-linejoin="round"><path d="M6.5 1 2.5 6.5h2.7L5 11l4-5.5H6.3L6.5 1Z" /></svg
				>
			{:else if item.icon === 'runtime'}
				<svg
					viewBox="0 0 12 12"
					fill="none"
					stroke="currentColor"
					stroke-width="1.4"
					stroke-linecap="round"
					stroke-linejoin="round"
					><circle cx="6" cy="6" r="4.5" /><line x1="6" y1="3.6" x2="6" y2="6.4" /><circle
						cx="6"
						cy="8.4"
						r="0.4"
						fill="currentColor"
						stroke="none"
					/></svg
				>
			{:else}
				<svg
					viewBox="0 0 12 12"
					fill="none"
					stroke="currentColor"
					stroke-width="1.4"
					stroke-linecap="round"
					stroke-linejoin="round"><polyline points="2.5,6.5 5,9 9.5,3.5" /></svg
				>
			{/if}
		</span>
		{#if detailBlock}
			<button
				class="activity-expand"
				onclick={() => onToggleCollapse(item, index)}
				aria-expanded={!isCollapsed(item, index)}
			>
				<span class="activity-label">{item.label}</span>
				<svg
					class="activity-chevron"
					class:open={!isCollapsed(item, index)}
					viewBox="0 0 12 12"
					fill="none"
					stroke="currentColor"
					stroke-width="1.6"
					stroke-linecap="round"
					stroke-linejoin="round"><polyline points="4,2 8,6 4,10" /></svg
				>
			</button>
		{:else}
			<span class="activity-label">{item.label}</span>
		{/if}
		{#if detailInline}<span class="activity-detail" title={detailInline}>{detailInline}</span>
		{/if}
		{#if item.streaming}<span class="activity-spinner" aria-hidden="true"></span>{/if}
	</div>
	{#if detailBlock && !isCollapsed(item, index)}
		<pre class="item item-activity-detail">{detailBlock}</pre>
	{/if}
{:else if item.kind === 'error'}
	<div class="item item-error" in:fly|local={enterAnim}>{item.message}</div>
{:else if item.kind === 'notice'}
	<div class="item item-notice" in:fly|local={enterAnim}>{item.message}</div>
{/if}

<style>
	.item {
		margin-bottom: var(--space-4);
	}

	/* ---- USER ---- */
	.item-user {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: var(--space-1);
	}

	.user-bubble {
		max-width: 560px;
		background: var(--user-bg);
		border: 1px solid var(--user-border);
		border-radius: var(--radius-lg);
		padding: var(--space-3) var(--space-4);
		/* Also carries .item-text for the markdown content styles; keep the
		   bubble's own compact metrics (item-text's 13.5px/1.75 is tuned for
		   long-form assistant prose, not a chat bubble). */
		font-size: 13px;
		line-height: 1.6;
		color: var(--text-primary);
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}

	/* Optimistic echo: slightly quieted until the committed Turn::Started
	 *  replaces it — reads as "sent, syncing" without a spinner's noise. */
	.item-user.pending .user-bubble {
		opacity: 0.72;
	}

	.pending-hint {
		font-size: 10.5px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		padding-right: var(--space-1);
	}

	/* ---- TURN ACTIONS (fork today; reserved slot for a future rating control,
	   DESIGN.md §4.7) ---- */
	/* Faint, low-frequency affordance: dim by default so it never competes with
	   the conversation, brightening only when its turn is hovered. Icon-only —
	   the meaning rides on the glyph + tooltip, no visible label. */
	.turn-actions {
		display: flex;
		align-items: center;
		opacity: 0.4;
		transition: opacity var(--dur-fast) var(--ease-out);
	}

	.item-user:hover .turn-actions,
	.turn-actions:focus-within {
		opacity: 1;
	}

	.turn-action-btn {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 20px;
		padding: 0;
		border: 1px solid transparent;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--text-disabled);
		cursor: pointer;
		transition:
			color var(--dur-fast) var(--ease-out),
			background var(--dur-fast) var(--ease-out),
			border-color var(--dur-fast) var(--ease-out);
	}

	.turn-action-btn:hover {
		color: var(--text-secondary);
		background: var(--canvas-float);
		border-color: var(--border-default);
	}

	.turn-action-icon {
		width: 12px;
		height: 12px;
		flex-shrink: 0;
	}

	@media (prefers-reduced-motion: reduce) {
		.turn-actions,
		.turn-action-btn {
			transition: none;
		}
	}

	/* ---- AGENT TEXT ---- */
	.item-text {
		font-size: 13.5px;
		line-height: 1.75;
		color: var(--text-primary);
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}

	.item-text :global(p) {
		margin-bottom: var(--space-3);
	}
	.item-text :global(p:last-child) {
		margin-bottom: 0;
	}
	.item-text :global(h1),
	.item-text :global(h2),
	.item-text :global(h3) {
		font-weight: 600;
		margin: 1em 0 0.5em;
		color: var(--text-primary);
		line-height: 1.3;
	}
	.item-text :global(strong) {
		font-weight: 600;
		color: var(--text-primary);
	}
	.item-text :global(code) {
		font-family: var(--font-mono);
		font-size: 12px;
		background: var(--canvas-float);
		color: var(--syntax-str);
		padding: 1px 5px;
		border-radius: 3px;
		border: 1px solid var(--border-subtle);
	}
	.item-text :global(.code-block) {
		margin: var(--space-3) 0;
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		overflow: hidden;
		background: var(--canvas-float);
	}
	.item-text :global(.code-lang) {
		font-family: var(--font-mono);
		font-size: 10px;
		font-weight: 510;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-tertiary);
		padding: 5px var(--space-3);
		background: var(--canvas-overlay);
		border-bottom: 1px solid var(--border-subtle);
		user-select: none;
	}
	.item-text :global(.code-block pre) {
		margin: 0;
		padding: var(--space-3);
		background: none;
		border: none;
		border-radius: 0;
		overflow-x: auto;
	}
	.item-text :global(pre code) {
		background: none;
		border: none;
		padding: 0;
		color: var(--text-secondary);
	}

	/* ---- CODE SYNTAX HIGHLIGHT (hljs token → design token) ---- */
	.item-text :global(.hljs-comment),
	.item-text :global(.hljs-quote) {
		color: var(--syntax-comment);
		font-style: italic;
	}
	.item-text :global(.hljs-keyword),
	.item-text :global(.hljs-selector-tag),
	.item-text :global(.hljs-literal),
	.item-text :global(.hljs-section),
	.item-text :global(.hljs-doctag),
	.item-text :global(.hljs-name) {
		color: var(--syntax-keyword);
	}
	.item-text :global(.hljs-string),
	.item-text :global(.hljs-regexp),
	.item-text :global(.hljs-meta .hljs-string) {
		color: var(--syntax-str);
	}
	.item-text :global(.hljs-number),
	.item-text :global(.hljs-bullet) {
		color: var(--syntax-num);
	}
	.item-text :global(.hljs-title),
	.item-text :global(.hljs-title.function_),
	.item-text :global(.hljs-function .hljs-title),
	.item-text :global(.hljs-built_in) {
		color: var(--syntax-fn);
	}
	.item-text :global(.hljs-type),
	.item-text :global(.hljs-class .hljs-title),
	.item-text :global(.hljs-attr),
	.item-text :global(.hljs-attribute),
	.item-text :global(.hljs-property) {
		color: var(--syntax-type);
	}
	.item-text :global(.hljs-variable),
	.item-text :global(.hljs-template-variable),
	.item-text :global(.hljs-symbol) {
		color: var(--syntax-key);
	}
	.item-text :global(.hljs-emphasis) {
		font-style: italic;
	}
	.item-text :global(.hljs-strong) {
		font-weight: 600;
	}

	/* ---- TABLES (GFM) ---- */
	.item-text :global(table) {
		border-collapse: collapse;
		width: max-content;
		max-width: 100%;
		margin: var(--space-3) 0;
		font-size: 12.5px;
		font-family: var(--font-sans);
		display: block;
		overflow-x: auto;
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
	}
	.item-text :global(th),
	.item-text :global(td) {
		border-right: 1px solid var(--border-subtle);
		border-bottom: 1px solid var(--border-subtle);
		padding: var(--space-2) var(--space-3);
		text-align: left;
		vertical-align: top;
		line-height: 1.5;
	}
	.item-text :global(tr > th:last-child),
	.item-text :global(tr > td:last-child) {
		border-right: none;
	}
	.item-text :global(tbody tr:last-child td) {
		border-bottom: none;
	}
	.item-text :global(thead th) {
		color: var(--text-primary);
		font-weight: 590;
		font-size: 11px;
		letter-spacing: 0.02em;
		border-bottom: 1px solid var(--border-default);
	}
	.item-text :global(tbody td) {
		color: var(--text-secondary);
	}
	.item-text :global(ol),
	.item-text :global(ul) {
		padding-left: var(--space-5);
		margin-bottom: var(--space-3);
	}
	.item-text :global(li) {
		margin-bottom: var(--space-1);
		padding-left: var(--space-1);
	}
	.item-text :global(li::marker) {
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
	}
	.item-text :global(a) {
		color: var(--accent-ink);
		text-decoration: none;
		border-bottom: 1px solid color-mix(in srgb, var(--accent) 30%, transparent);
		transition: border-color var(--dur-fast);
	}
	.item-text :global(a:hover) {
		border-color: var(--accent);
	}
	.item-text :global(blockquote) {
		border-left: 2px solid var(--border-strong);
		padding-left: var(--space-3);
		color: var(--text-secondary);
		margin: var(--space-3) 0;
	}

	/* Streaming cursor on the live text item */
	.item-text.streaming::after {
		content: '';
		display: inline-block;
		width: 2px;
		height: 1em;
		background: var(--accent);
		vertical-align: text-bottom;
		margin-left: 2px;
		border-radius: 1px;
		animation: cursor-blink 1.1s step-end infinite;
	}

	@keyframes cursor-blink {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0;
		}
	}

	/* ---- REASONING (inline, non-card) ---- */
	.reasoning-inline-label {
		font-size: 12px;
		color: var(--text-tertiary);
		font-style: italic;
	}

	/* The collapsed streaming line: the latest reasoning line rolling by is
	 *  the liveness cue. Flex item so it ellipsizes instead of pushing the
	 *  label/dot off the row. */
	.reasoning-ticker {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* Streaming body: the live reasoning text, quiet (muted + smaller) but
	 *  visible — the user watches it think. Same muted weight as the done
	 *  state, just not collapsed. */
	.reasoning-stream {
		font-size: 12.5px;
		color: var(--text-tertiary);
		line-height: 1.7;
		font-family: var(--font-chinese);
		text-wrap: pretty;
		opacity: 0.75;
		margin-top: 2px;
	}
	.reasoning-stream :global(p) {
		margin-bottom: var(--space-2);
	}
	.reasoning-stream :global(p:last-child) {
		margin-bottom: 0;
	}
	.reasoning-stream :global(ol),
	.reasoning-stream :global(ul) {
		padding-left: var(--space-5);
		margin-bottom: var(--space-2);
	}
	.reasoning-stream :global(code) {
		font-family: var(--font-mono);
		font-size: 11.5px;
		background: var(--canvas-float);
		padding: 1px 4px;
		border-radius: 3px;
	}

	/* Single-line muted preview, click to expand. Streaming renders as flex
	 *  (label + dot + ticker line); done renders as a plain text block. */
	.reasoning-preview {
		display: block;
		width: 100%;
		padding: var(--space-1) 0;
		background: none;
		border: none;
		cursor: pointer;
		text-align: left;
		font-size: 12px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		font-style: italic;
		opacity: 0.62;
		transition: opacity var(--dur-fast) var(--ease-out);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.reasoning-preview:hover {
		opacity: 1;
	}

	.item-reasoning.streaming .reasoning-preview {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.reasoning-body {
		padding: var(--space-2) 0 var(--space-2) var(--space-3);
		margin-top: 2px;
		margin-left: var(--space-3);
		border-left: 1px solid var(--border-subtle);
		font-size: 12.5px;
		color: var(--text-tertiary);
		line-height: 1.7;
		font-family: var(--font-chinese);
		text-wrap: pretty;
		opacity: 0.62;
	}
	.reasoning-body :global(p) {
		margin-bottom: var(--space-2);
	}
	.reasoning-body :global(p:last-child) {
		margin-bottom: 0;
	}
	.reasoning-body :global(ol),
	.reasoning-body :global(ul) {
		padding-left: var(--space-5);
		margin-bottom: var(--space-2);
	}
	.reasoning-body :global(li) {
		margin-bottom: 2px;
	}
	.reasoning-body :global(li::marker) {
		color: var(--text-disabled);
		font-variant-numeric: tabular-nums;
	}
	.reasoning-body :global(code) {
		font-family: var(--font-mono);
		font-size: 11.5px;
		background: var(--canvas-float);
		padding: 1px 4px;
		border-radius: 3px;
	}

	/* ---- ERROR / NOTICE ---- */
	.item-error {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		border: 1px solid color-mix(in srgb, var(--state-error) 30%, transparent);
		border-radius: var(--radius-md);
		padding: var(--space-3) var(--space-4);
		font-size: 12.5px;
	}

	.item-notice {
		color: var(--text-tertiary);
		font-size: 12px;
		font-style: italic;
		text-align: center;
		padding: var(--space-2) var(--space-4);
		font-family: var(--font-chinese);
	}

	/* ---- ACTIVITY ROW (todo op / hook / runtime reminder trace) ---- */
	/* A quiet rail row, not a card: a hairline guide on the left, a small icon
	   on it, then the one-line label flush-left with the conversation's text
	   column. The variable-length why (hook block reason, reminder text) rides
	   as a chip so a long reason never swallows the fixed-shape label. */
	.item-activity {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		color: var(--text-tertiary);
		font-size: 12px;
		padding: 1px 0 1px var(--space-4);
		margin-bottom: var(--space-3);
		font-family: var(--font-chinese);
		position: relative;
	}

	/* The rail: a hairline connecting the row to the flow above/below. */
	.item-activity::before {
		content: '';
		position: absolute;
		left: calc(var(--space-4) - var(--space-2) - 5px);
		top: 0;
		bottom: 0;
		width: 1px;
		background: color-mix(in srgb, var(--text-tertiary) 28%, transparent);
	}

	.activity-mark {
		position: relative;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 14px;
		height: 14px;
		flex: none;
		/* Sit the icon on the rail: offset left so its center meets the line,
		   with a canvas-colored halo so the line doesn't strike through it. */
		margin-left: calc(-1 * var(--space-2) - 6px);
		margin-right: var(--space-1);
		background: var(--canvas-base);
		border-radius: 50%;
	}

	.activity-mark svg {
		width: 11px;
		height: 11px;
	}

	.activity-label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		flex: none;
		max-width: 46ch;
	}

	.activity-detail {
		font-size: 11px;
		color: var(--text-tertiary);
		background: color-mix(in srgb, var(--text-tertiary) 10%, transparent);
		border-radius: var(--radius-sm);
		padding: 0 var(--space-2);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		max-width: 34ch;
		flex: none;
	}

	/* A hook that blocked/failed (detail = the reason): tint the chip, not the
	   whole row — the row stays quiet, the why reads as the warning. */
	.activity-blocked .activity-detail,
	.activity-blocked .activity-label {
		color: var(--warning);
	}

	.activity-blocked .activity-detail {
		background: color-mix(in srgb, var(--warning) 12%, transparent);
	}

	.activity-spinner {
		width: 9px;
		height: 9px;
		flex: none;
		border: 1.4px solid color-mix(in srgb, currentColor 35%, transparent);
		border-top-color: currentColor;
		border-radius: 50%;
		animation: activity-spin 0.7s linear infinite;
	}

	@keyframes activity-spin {
		to {
			transform: rotate(360deg);
		}
	}

	/* Expandable detail (a multi-line runtime reminder): the row's label is a
	   button with a chevron; the full text unfolds below, indented to the
	   label column so it reads as belonging to the row above. */
	.activity-expand {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: 0;
		border: none;
		background: none;
		color: inherit;
		font: inherit;
		cursor: pointer;
		min-width: 0;
	}

	.activity-expand:hover .activity-label {
		color: var(--text-secondary);
	}

	.activity-chevron {
		width: 9px;
		height: 9px;
		flex: none;
		transition: transform var(--dur-fast) var(--ease-out);
	}

	.activity-chevron.open {
		transform: rotate(90deg);
	}

	.item-activity-detail {
		margin: calc(-1 * var(--space-2)) 0 var(--space-3);
		padding: var(--space-2) var(--space-3);
		padding-left: calc(var(--space-4) + 10px);
		font-size: 11.5px;
		line-height: 1.6;
		color: var(--text-tertiary);
		font-family: var(--font-mono);
		white-space: pre-wrap;
		word-break: break-word;
		background: color-mix(in srgb, var(--text-tertiary) 6%, transparent);
		border-radius: var(--radius-sm);
	}

	/* Streaming dot (reasoning / tool live indicator) */
	.streaming-dot {
		display: inline-block;
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--accent);
		animation: pulse 1s ease-in-out infinite;
		flex-shrink: 0;
	}

	@keyframes pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.3;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.item-text.streaming::after {
			animation: none;
		}
		.streaming-dot {
			animation: none;
		}
	}
</style>
