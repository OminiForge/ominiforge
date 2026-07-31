<script lang="ts">
	/** Sticky save bar for the settings page: surfaces the moment any section
	 *  has unsaved edits and stays pinned to the bottom of the scrollport, so
	 *  "save" is never scrolled out of reach (the old bottom-of-form button was
	 *  invisible once a list grew). Rendered only while dirty. */
	let {
		dirtyCount,
		saving = false,
		onsave,
		ondiscard
	}: {
		dirtyCount: number;
		saving?: boolean;
		onsave: () => void;
		ondiscard: () => void;
	} = $props();
</script>

{#if dirtyCount > 0}
	<div class="savebar" role="status">
		<span class="msg">{dirtyCount} unsaved change(s)</span>
		<div class="actions">
			<button class="btn ghost" onclick={ondiscard} disabled={saving}>Discard</button>
			<button class="btn primary" onclick={onsave} disabled={saving}>
				{saving ? 'Saving…' : 'Save all'}
			</button>
		</div>
	</div>
{/if}

<style>
	/* Sticky bottom: the bar's natural slot is the end of the settings page, so
	   the bottom constraint pulls it up into view at all times — pinned to the
	   scrollport bottom while any part of the page is scrolled. */
	.savebar {
		position: sticky;
		bottom: var(--space-4);
		margin-top: var(--space-6);
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		padding: var(--space-3) var(--space-4);
		background: var(--canvas-float);
		border: 1px solid var(--border-strong);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
		z-index: var(--z-sticky, 10);
	}

	.msg {
		font-family: var(--font-chinese);
		font-size: 12px;
		color: var(--text-secondary);
	}

	.actions {
		display: flex;
		gap: var(--space-2);
	}

	.btn {
		padding: 6px var(--space-4);
		border-radius: var(--radius-sm);
		font-size: 13px;
		font-weight: 590;
		font-family: var(--font-chinese);
		cursor: pointer;
		border: 1px solid transparent;
	}

	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}

	.btn.primary {
		background: var(--accent);
		color: var(--accent-fg);
	}

	.btn.primary:hover:not(:disabled) {
		background: var(--accent-hover);
	}

	.btn.ghost {
		background: transparent;
		color: var(--text-secondary);
		border-color: var(--border-default);
	}

	.btn.ghost:hover:not(:disabled) {
		color: var(--text-primary);
		border-color: var(--border-strong);
	}
</style>
