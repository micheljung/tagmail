<script lang="ts">
	const mask = $props.id();
</script>

<a class="logo" href="/inbox" aria-label="Tagmail, go to Inbox">
	<svg viewBox="0 0 64 64" width="30" height="30" aria-hidden="true">
		<mask id={mask} maskUnits="userSpaceOnUse" x="0" y="0" width="64" height="64">
			<path
				class="shape"
				fill="#fff"
				stroke="#fff"
				d="M24 12H51a7 7 0 0 1 7 7v26a7 7 0 0 1-7 7H24L7 32Z"
			/>
			<path class="seam" d="M19 7 41 29 63 7" />
			<circle cx="17" cy="32" r="3.6" />
		</mask>
		<path
			class="shape mark"
			mask="url(#{mask})"
			d="M24 12H51a7 7 0 0 1 7 7v26a7 7 0 0 1-7 7H24L7 32Z"
		/>
	</svg>
	<span class="word">tagmail</span>
</a>

<style>
	.logo {
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--ink);
		text-decoration: none;
	}

	.shape {
		stroke-width: 5;
		stroke-linejoin: round;
	}

	.mark {
		fill: var(--accent);
		stroke: var(--accent);
	}

	.seam {
		fill: none;
		stroke: #000;
		stroke-width: 4.5;
	}

	.word {
		font-size: 21px;
		font-weight: 600;
		letter-spacing: -0.025em;
	}
</style>
