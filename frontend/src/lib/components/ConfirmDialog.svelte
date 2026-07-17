<script lang="ts">
	import type { Snippet } from 'svelte';
	import Button from './Button.svelte';

	let {
		title,
		confirmLabel = '确认',
		cancelLabel = '取消',
		danger = false,
		error = null,
		onconfirm,
		oncancel,
		children
	}: {
		title: string;
		confirmLabel?: string;
		cancelLabel?: string;
		/** Style the confirm button as destructive (permanent delete). */
		danger?: boolean;
		/** Failure from the last confirm attempt, shown inside the dialog. */
		error?: string | null;
		onconfirm: () => void;
		oncancel: () => void;
		children?: Snippet;
	} = $props();

	// Focus the panel on mount so Escape/Enter are captured immediately and the
	// dialog is keyboard-reachable without stealing focus into a specific button.
	let panelEl = $state<HTMLDivElement | null>(null);
	$effect(() => {
		panelEl?.focus();
	});

	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.stopPropagation();
			oncancel();
		} else if (e.key === 'Enter' && !danger) {
			// Enter-to-confirm is withheld from destructive dialogs: an
			// irreversible action takes an explicit click, not a stray keypress.
			e.stopPropagation();
			onconfirm();
		}
	}
</script>

<svelte:window {onkeydown} />

<!-- Backdrop: a click outside the panel cancels. The inner panel stops
     propagation so clicks inside never dismiss. Keyboard users get the same
     dismissal via the window-level Escape handler above, so the backdrop click
     is a pointer-only convenience. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="backdrop" role="presentation" onclick={oncancel}>
	<div
		bind:this={panelEl}
		class="panel"
		role="alertdialog"
		aria-modal="true"
		aria-label={title}
		tabindex="-1"
		onclick={(e) => e.stopPropagation()}
	>
		<h2 class="title">{title}</h2>
		{#if children}
			<div class="body">{@render children()}</div>
		{/if}
		{#if error}
			<p class="error" role="alert">{error}</p>
		{/if}
		<div class="actions">
			<Button variant="ghost" onclick={oncancel}>{cancelLabel}</Button>
			<Button variant={danger ? 'danger' : 'accent'} onclick={onconfirm}>
				{confirmLabel}
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
		padding: var(--space-4, 16px);
	}

	.panel {
		background: var(--canvas-raised);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-lg, 12px);
		padding: var(--space-5, 20px);
		max-width: 380px;
		width: 100%;
		box-shadow: var(--shadow-lg);
	}

	.title {
		margin: 0 0 var(--space-2, 8px);
		font-size: 15px;
		font-weight: 600;
		color: var(--text-primary);
		font-family: var(--font-chinese);
	}

	.body {
		margin-bottom: var(--space-4, 16px);
		font-size: 13px;
		line-height: 1.5;
		color: var(--text-secondary);
		font-family: var(--font-chinese);
	}

	.error {
		margin: 0 0 var(--space-4, 16px);
		font-size: 13px;
		color: var(--state-error-text);
		font-family: var(--font-chinese);
	}

	.actions {
		display: flex;
		justify-content: flex-end;
		gap: var(--space-2, 8px);
	}
</style>
