<script lang="ts" module>
	/** One selectable entry in a `ModelSelect` list. */
	export interface SelectOption {
		/** The value emitted on select (empty string = the "default" row). */
		value: string;
		/** Primary label (model id / tier name / profile name). */
		label: string;
		/** Secondary text (provider name / description), rendered muted. */
		detail?: string;
	}
</script>

<script lang="ts">
	import { rise } from '$lib/motion';
	import { fly } from 'svelte/transition';

	/** A token-themed dropdown that stays dark in dark mode (a native
	 *  `<select>` renders its option list with OS colors — white rows on the
	 *  near-black console). The trigger IS the current value; clicking it opens
	 *  the option list directly (one click to open, one to pick). Used by the
	 *  session config pickers and the settings profile editor (DESIGN.md §4.2).
	 *
	 *  Two-way bindable `value`; the empty string maps to the caller's fallback
	 *  row. In `listOnly` mode (the session pickers) only the option list
	 *  renders — the host supplies its own trigger button. */
	let {
		options,
		value = $bindable(''),
		onselect,
		placeholder = 'Select…',
		up = false,
		disabled = false,
		listOnly = false,
		onclose
	}: {
		options: SelectOption[];
		value: string;
		/** Called with the new value after `value` updates (for hosts that
		 *  convert the empty string back to `null`, e.g. profile fields). */
		onselect?: (value: string) => void;
		placeholder?: string;
		/** Open upward (input-area pickers at the screen bottom). */
		up?: boolean;
		disabled?: boolean;
		/** Render only the option list (the host owns the trigger). The list is
		 *  always visible in this mode; `onclose` fires after a pick. */
		listOnly?: boolean;
		onclose?: () => void;
	} = $props();

	let open = $state(false);
	let root = $state<HTMLDivElement | null>(null);

	const current = $derived(options.find((o) => o.value === value));

	function pick(v: string) {
		value = v;
		onselect?.(v);
		open = false;
		onclose?.();
	}

	// Click outside closes: in the normal mode it just closes the list; in
	// `listOnly` mode (the host owns the trigger) it notifies the host to
	// unmount the list. Listened on `click` (after `pointerdown`): the trigger
	// that opens a picker stops pointerdown propagation, so a press on the
	// trigger TOGGLES via the host's handler while a click anywhere else —
	// including anywhere outside the list — closes it.
	function onWindowClick(e: MouseEvent) {
		if (listOnly) {
			if (root && !root.contains(e.target as Node)) onclose?.();
			return;
		}
		if (open && root && !root.contains(e.target as Node)) open = false;
	}

	function onKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			open = false;
			onclose?.();
		}
	}
</script>

<svelte:window onclick={onWindowClick} />

{#if listOnly}
	<div class="msel-list inline" role="listbox" bind:this={root}>
		{#each options as o (o.value)}
			<button
				type="button"
				class="msel-opt"
				class:selected={o.value === value}
				role="option"
				aria-selected={o.value === value}
				onclick={() => pick(o.value)}
			>
				<span class="msel-opt-label">{o.label}</span>
				{#if o.detail}<span class="msel-opt-detail">{o.detail}</span>{/if}
				{#if o.value === value}
					<svg
						class="msel-check"
						width="11"
						height="11"
						viewBox="0 0 11 11"
						fill="none"
						stroke="currentColor"
						stroke-width="1.6"
						stroke-linecap="round"
						stroke-linejoin="round"
						aria-hidden="true"
					>
						<polyline points="2,5.8 4.4,8.2 9,2.8" />
					</svg>
				{/if}
			</button>
		{/each}
	</div>
{:else}
<div class="msel" class:up bind:this={root}>
	<button
		type="button"
		class="msel-trigger"
		class:on={open}
		{disabled}
		aria-expanded={open}
		aria-haspopup="listbox"
		onclick={() => (open = !open)}
		onkeydown={onKeydown}
	>
		<span class="msel-value" class:placeholder={!current}>
			{current ? current.label : placeholder}
		</span>
		{#if current?.detail}<span class="msel-detail">{current.detail}</span>{/if}
		<svg
			class="msel-chev"
			class:open
			width="10"
			height="10"
			viewBox="0 0 10 10"
			fill="none"
			stroke="currentColor"
			stroke-width="1.5"
			stroke-linecap="round"
			stroke-linejoin="round"
			aria-hidden="true"
		>
			<polyline points="2,3.5 5,6.5 8,3.5" />
		</svg>
	</button>

	{#if open}
		<div class="msel-list" role="listbox" transition:fly={rise(up ? 6 : -6)}>
			{#each options as o (o.value)}
				<button
					type="button"
					class="msel-opt"
					class:selected={o.value === value}
					role="option"
					aria-selected={o.value === value}
					onclick={() => pick(o.value)}
				>
					<span class="msel-opt-label">{o.label}</span>
					{#if o.detail}<span class="msel-opt-detail">{o.detail}</span>{/if}
					{#if o.value === value}
						<svg
							class="msel-check"
							width="11"
							height="11"
							viewBox="0 0 11 11"
							fill="none"
							stroke="currentColor"
							stroke-width="1.6"
							stroke-linecap="round"
							stroke-linejoin="round"
							aria-hidden="true"
						>
							<polyline points="2,5.8 4.4,8.2 9,2.8" />
						</svg>
					{/if}
				</button>
			{/each}
		</div>
	{/if}
</div>
{/if}

<style>
	.msel {
		position: relative;
		width: 100%;
	}

	.msel-trigger {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: 5px 8px;
		background: var(--canvas-base);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		color: var(--text-primary);
		font-family: var(--font-mono);
		font-size: 12px;
		text-align: left;
		cursor: pointer;
		transition:
			border-color var(--motion-fast),
			background var(--motion-fast);
	}

	.msel-trigger:hover:not(:disabled),
	.msel-trigger.on {
		border-color: var(--border-strong);
	}

	.msel-trigger:focus-visible {
		outline: none;
		border-color: var(--border-strong);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 10%, transparent);
	}

	.msel-trigger:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	.msel-value {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.msel-value.placeholder {
		color: var(--text-tertiary);
	}

	.msel-detail {
		flex-shrink: 0;
		color: var(--text-tertiary);
		font-size: 11px;
	}

	.msel-chev {
		flex-shrink: 0;
		color: var(--text-tertiary);
		transition: transform var(--motion-fast);
	}

	.msel-chev.open {
		transform: rotate(180deg);
	}

	/* The option list: float surface, one step above the overlay popover it
	   usually sits in (surface ladder §2). All colors are tokens, so the list
	   follows the theme instead of the OS. */
	.msel-list {
		position: absolute;
		top: calc(100% + 4px);
		left: 0;
		right: 0;
		z-index: var(--z-popover);
		max-height: 240px;
		overflow-y: auto;
		padding: 4px;
		background: var(--canvas-float);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-md);
	}

	.msel.up .msel-list {
		top: auto;
		bottom: calc(100% + 4px);
	}

	/* listOnly mode: a static list (the host positions it), no absolute
	   placement or shadow of its own. */
	.msel-list.inline {
		position: static;
		max-height: 220px;
	}

	.msel-opt {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: 6px 8px;
		background: none;
		border: none;
		border-radius: var(--radius-sm);
		color: var(--text-primary);
		font-family: var(--font-mono);
		font-size: 12px;
		text-align: left;
		cursor: pointer;
		transition: background var(--motion-fast);
	}

	.msel-opt:hover {
		background: var(--canvas-overlay);
	}

	.msel-opt.selected {
		background: var(--accent-dim);
	}

	.msel-opt-label {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.msel-opt-detail {
		flex-shrink: 0;
		color: var(--text-tertiary);
		font-size: 11px;
	}

	.msel-check {
		flex-shrink: 0;
		color: var(--accent-ink);
	}
</style>
