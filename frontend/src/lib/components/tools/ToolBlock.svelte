<script lang="ts">
	import type { Item } from '$lib/conversation';
	import { resultComponent } from '$lib/tools/registry';
	import { extractArgsPath } from '$lib/tools/utils';
	import ApprovalControls from '$lib/components/ApprovalControls.svelte';
	import type { ApprovalScope } from '$lib/types/ApprovalScope';

	/** The 120% tool card (DESIGN.md §4.1): a collapsible three-state shell (pip +
	 *  name + status badge + arg preview + chevron) whose body is dispatched to a
	 *  per-tool result component by name.  All tools stay open after completion;
	 *  `read` auto-collapses.  The user's toggle overrides and persists across
	 *  status flips.
	 *
	 *  A permission `ask` suspends the call *inside this card* (`approvalPending`):
	 *  the header grows ApprovalControls and the badge reads 等待批准; the decision
	 *  resolves back through the same card (approved → it runs; rejected → the
	 *  paired Tool::Failed lands it in error). No separate approval card. */
	let {
		item,
		onDecide
	}: {
		item: Item & { kind: 'tool' };
		/** Answer a permission `ask` with a decision + scope (Conversation wires
		 *  `client.approve` through this). Optional: history renders read-only. */
		onDecide?: (
			callId: string,
			decision: 'approve' | 'reject',
			scope: ApprovalScope
		) => void | Promise<void>;
	} = $props();

	// Tools stay open after completion, except `read` which auto-collapses. A
	// pending approval always stays open — its prompt is in the header.
	let override = $state<boolean | null>(null);
	const expanded = $derived(
		override ??
			(item.approvalPending === true || !(item.name === 'read' && item.status !== 'running'))
	);
	const Body = $derived(resultComponent(item.name));

	const awaiting = $derived(item.approvalPending === true);

	function clip(s: string, n: number): string {
		const line = s.split('\n')[0];
		return line.length > n ? line.slice(0, n) + '…' : line;
	}

	/** One-line preview of the call's args for the collapsed header — the most
	 *  meaningful field (command/path/query/…) from the parsed JSON, else raw. */
	function toolPreview(args: string): string {
		if (!args || args === '{}') return '';
		let parsed: unknown;
		try {
			parsed = JSON.parse(args);
		} catch {
			return clip(args, 80);
		}
		if (parsed && typeof parsed === 'object') {
			const obj = parsed as Record<string, unknown>;
			const keys = [
				'command',
				'cmd',
				'script',
				'path',
				'file',
				'file_path',
				'query',
				'url',
				'pattern'
			];
			for (const k of keys) {
				if (typeof obj[k] === 'string' && obj[k]) return clip(obj[k] as string, 80);
			}
			// `edit`'s batch args carry no top-level path key — the path is nested
			// under `edits[0]`. Reuse the shared extractor (which knows that shape)
			// so the collapsed header shows the target file, not truncated raw JSON.
			const path = extractArgsPath(args);
			if (path) return clip(path, 80);
			const firstStr = Object.values(obj).find((v) => typeof v === 'string' && v);
			if (typeof firstStr === 'string') return clip(firstStr, 80);
		}
		return clip(args, 80);
	}

	const preview = $derived(toolPreview(item.args));
</script>

<div
	class="tool-block"
	class:done={item.status === 'done'}
	class:running={item.status === 'running'}
	class:error={item.status === 'error'}
	class:awaiting
	class:expanded
>
	<div class="tool-header">
		<button class="tool-toggle" onclick={() => (override = !expanded)} aria-expanded={expanded}>
			<span class="tool-pip"></span>
			{#if item.status === 'running' && !awaiting}
				<span class="tool-spinner"></span>
			{/if}
			<span class="tool-name">{item.name}</span>
			<span class="tool-status-badge">{awaiting ? '等待批准' : item.status}</span>
			{#if preview}
				<span class="tool-preview">{preview}</span>
			{/if}
			<svg
				class="tool-chevron"
				viewBox="0 0 12 12"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				stroke-linecap="round"
				stroke-linejoin="round"
			>
				<polyline points="4,2 8,6 4,10" />
			</svg>
		</button>
		{#if awaiting && item.callId && onDecide}
			<ApprovalControls callId={item.callId} {onDecide} />
		{/if}
	</div>
	{#if expanded}
		<div class="tool-detail">
			<Body
				name={item.name}
				args={item.args}
				result={item.result}
				diagnostics={item.diagnostics}
				status={item.status}
				error_code={item.error_code}
				view={item.view}
				preview={item.approvalPending ? item.preview : undefined}
			/>
			{#if item.status === 'running' && !awaiting}
				<div class="running-indicator">
					<span class="tool-spinner"></span>
					<span>正在执行…</span>
				</div>
			{/if}
		</div>
	{/if}
</div>

<style>
	.tool-block {
		border-radius: var(--radius-md);
		overflow: hidden;
		border: 1px solid var(--border-subtle);
		transition: border-color var(--dur-std) var(--ease-out);
	}
	.tool-block.done {
		border-color: color-mix(in srgb, var(--state-done) 22%, transparent);
	}
	.tool-block.running {
		border-color: color-mix(in srgb, var(--state-running) 35%, transparent);
		animation: tool-running-pulse 2s ease-in-out infinite;
	}
	@keyframes tool-running-pulse {
		0%,
		100% {
			border-color: color-mix(in srgb, var(--state-running) 30%, transparent);
			box-shadow: 0 0 0 0 transparent;
		}
		50% {
			border-color: color-mix(in srgb, var(--state-running) 55%, transparent);
			box-shadow: 0 0 0 3px color-mix(in srgb, var(--state-running) 6%, transparent);
		}
	}
	.tool-block.error {
		border-color: color-mix(in srgb, var(--state-error) 30%, transparent);
	}

	.tool-header {
		display: flex;
		align-items: center;
		background: var(--canvas-overlay);
		user-select: none;
		transition: background var(--dur-fast) var(--ease-out);
	}
	.tool-header:hover {
		background: var(--canvas-float);
	}
	/* The toggle is a real <button> nested next to (not around) the approval
	   controls — interactive elements must not nest. */
	.tool-toggle {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 7px var(--space-3);
		cursor: pointer;
		text-align: left;
	}
	.tool-header > :global(.controls) {
		margin-right: var(--space-2);
	}

	.tool-pip {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		flex-shrink: 0;
		position: relative;
		background: var(--text-tertiary);
	}
	.done .tool-pip {
		background: var(--state-done);
	}
	.running .tool-pip {
		background: var(--state-running);
	}
	.awaiting .tool-pip {
		background: var(--state-running);
	}
	.error .tool-pip {
		background: var(--state-error);
	}
	.running .tool-pip::after,
	.awaiting .tool-pip::after {
		content: '';
		position: absolute;
		inset: -3px;
		border-radius: 50%;
		border: 1.5px solid var(--state-running);
		opacity: 0;
		animation: pip-ripple 1.8s ease-out infinite;
	}
	@keyframes pip-ripple {
		0% {
			opacity: 0.7;
			transform: scale(0.5);
		}
		100% {
			opacity: 0;
			transform: scale(2.2);
		}
	}

	.tool-spinner {
		width: 11px;
		height: 11px;
		border: 1.5px solid color-mix(in srgb, var(--state-running) 25%, transparent);
		border-top-color: var(--state-running);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
		flex-shrink: 0;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	.tool-name {
		font-family: var(--font-mono);
		font-size: 12px;
		font-weight: 500;
		color: var(--text-primary);
		letter-spacing: -0.01em;
		flex-shrink: 0;
	}

	.tool-status-badge {
		font-size: 10px;
		font-weight: 510;
		padding: 1px 5px;
		border-radius: 3px;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		flex-shrink: 0;
	}
	.done .tool-status-badge {
		background: var(--state-done-bg);
		color: var(--state-done-text);
		border: 1px solid color-mix(in srgb, var(--state-done) 25%, transparent);
	}
	.running .tool-status-badge {
		background: var(--state-running-bg);
		color: var(--state-running-text);
		border: 1px solid color-mix(in srgb, var(--state-running) 25%, transparent);
	}
	.error .tool-status-badge {
		background: var(--state-error-bg);
		color: var(--state-error-text);
		border: 1px solid color-mix(in srgb, var(--state-error) 25%, transparent);
	}

	.tool-preview {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-tertiary);
		font-family: var(--font-mono);
		font-size: 11px;
	}

	.tool-chevron {
		margin-left: auto;
		width: 12px;
		height: 12px;
		color: var(--text-tertiary);
		transition: transform var(--dur-std) var(--ease-out);
		flex-shrink: 0;
	}
	.tool-block.expanded .tool-chevron {
		transform: rotate(90deg);
	}

	.tool-detail {
		background: var(--canvas-base);
		border-top: 1px solid var(--border-subtle);
	}

	.running-indicator {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		font-size: 12px;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		padding: var(--space-2) var(--space-4);
		border-top: 1px solid var(--border-subtle);
	}

	@media (prefers-reduced-motion: reduce) {
		.tool-block.running,
		.tool-spinner,
		.running .tool-pip::after {
			animation: none;
		}
	}
</style>
