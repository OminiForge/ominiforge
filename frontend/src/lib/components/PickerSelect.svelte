<script lang="ts">
	import { fly } from 'svelte/transition';
	import { rise } from '$lib/motion';
	import ModelSelect, { type SelectOption } from './ModelSelect.svelte';

	/** A config picker: a compact `KEY value` trigger that opens a token-themed
	 *  option list (ModelSelect `listOnly`) — one click to open, one to pick
	 *  (DESIGN.md §4.2). Unifies the trigger + popover + open/close wiring so a
	 *  picker anywhere renders and behaves the same.
	 *
	 *  Placement is automatic: on open the trigger's viewport rect is measured
	 *  and the popover flips to whichever side has room (below/above, and
	 *  left/right edge alignment), so it never runs off the viewport regardless
	 *  of where the host places the trigger. Callers pass no positioning props. */
	let {
		options,
		value = $bindable(''),
		key,
		label,
		title,
		disabled = false,
		onselect
	}: {
		options: SelectOption[];
		/** Two-way bound selected value (empty string = the host's fallback row). */
		value: string;
		/** Mono uppercase category key shown on the trigger (e.g. `model`). */
		key: string;
		/** The human-readable current value shown on the trigger. */
		label: string;
		/** Trigger tooltip. */
		title?: string;
		disabled?: boolean;
		onselect?: (value: string) => void;
	} = $props();

	let open = $state(false);
	let triggerEl = $state<HTMLButtonElement | null>(null);
	// Placement, recomputed each open. `up` = open above the trigger; `left` =
	// align the popover's left edge to the trigger (else its right edge).
	let up = $state(false);
	let left = $state(false);

	const POPOVER_W = 300;
	const POPOVER_MAX_H = 240; // matches ModelSelect's option-list cap
	const GAP = 6;

	function toggle(e: MouseEvent) {
		e.stopPropagation();
		if (!open) measure();
		open = !open;
	}

	/** Flip the popover to the side with room, from the trigger's viewport
	 *  rect. Called on open so any host position (composer bottom, form-row
	 *  left, …) lands the list on-screen without a positioning prop. */
	function measure() {
		if (!triggerEl) return;
		const r = triggerEl.getBoundingClientRect();
		// Vertical: prefer below; flip up when there isn't room below but is above.
		up = r.bottom + GAP + POPOVER_MAX_H > window.innerHeight && r.top > POPOVER_MAX_H + GAP;
		// Horizontal: prefer right-edge alignment (extends left); flip to
		// left-edge alignment when the trigger is too close to the left edge for
		// a 300px list to extend leftward.
		left = r.left < POPOVER_W - r.width;
	}

	function pick(v: string) {
		value = v;
		onselect?.(v);
		open = false;
	}
</script>

<div class="picker">
	<button
		bind:this={triggerEl}
		class="picker-trigger"
		class:on={open}
		{disabled}
		{title}
		aria-expanded={open}
		aria-haspopup="listbox"
		onclick={toggle}
	>
		<span class="picker-key">{key}</span>
		<span class="picker-label">{label}</span>
	</button>
	{#if open}
		<div
			class="picker-popover"
			class:up
			class:left
			transition:fly={rise(up ? 6 : -6)}
		>
			<ModelSelect {options} bind:value listOnly onselect={pick} onclose={() => (open = false)} />
		</div>
	{/if}
</div>

<style>
	.picker {
		position: relative;
		display: flex;
		flex-shrink: 1;
		min-width: 0;
	}

	/* The trigger IS the current value: a quiet mono `KEY` category label plus
	   the readable value, on a hairline chip. One click opens the list. */
	.picker-trigger {
		display: flex;
		align-items: center;
		gap: 6px;
		max-width: 190px;
		padding: 5px 10px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border-default);
		background: transparent;
		color: var(--text-secondary);
		font-size: 11.5px;
		font-weight: 450;
		cursor: pointer;
		font-family: var(--font-sans);
		transition: all var(--dur-fast) var(--ease-out);
	}

	.picker-trigger:hover:not(:disabled),
	.picker-trigger.on {
		background: var(--surface-hover);
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.picker-trigger:active:not(:disabled) {
		transform: translateY(1px);
	}

	.picker-trigger:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	/* mono lowercase category key (profile / model / effort): quiet label, the
	   value is the readable part. */
	.picker-key {
		flex-shrink: 0;
		font-family: var(--font-mono);
		font-size: 9.5px;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-tertiary);
	}

	.picker-label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--font-mono);
		font-size: 11px;
	}

	/* The option list. Edge alignment + direction are set from the measured
	   trigger rect (see `measure`), never hardcoded — so the list stays on
	   screen wherever the trigger sits. Capped and scrolled by ModelSelect. */
	.picker-popover {
		position: absolute;
		top: calc(100% + 6px);
		right: 0;
		z-index: var(--z-popover);
		width: 300px;
		max-width: min(300px, 80vw);
	}

	.picker-popover.left {
		right: auto;
		left: 0;
	}

	.picker-popover.up {
		top: auto;
		bottom: calc(100% + 6px);
	}
</style>
