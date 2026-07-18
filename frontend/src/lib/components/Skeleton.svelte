<script lang="ts">
	/** Placeholder block shown while content loads. Size via `width`/`height`
	 *  (any CSS length); shape via `radius`. The shimmer sweep uses only canvas
	 *  tokens and is stilled under prefers-reduced-motion. */
	let {
		width = '100%',
		height = '14px',
		radius = 'var(--radius-sm)'
	}: { width?: string; height?: string; radius?: string } = $props();
</script>

<div class="skel" style:width style:height style:border-radius={radius} aria-hidden="true"></div>

<style>
	.skel {
		background: linear-gradient(
			100deg,
			var(--canvas-raised) 40%,
			var(--canvas-float) 50%,
			var(--canvas-raised) 60%
		);
		background-size: 200% 100%;
		animation: skel-sweep 1.4s linear infinite;
	}

	@keyframes skel-sweep {
		from {
			background-position: 150% 0;
		}
		to {
			background-position: -50% 0;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.skel {
			animation: none;
		}
	}
</style>
