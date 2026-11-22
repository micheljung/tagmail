<a class="logo" href="/inbox" aria-label="Tagmail, go to Inbox">
	<svg viewBox="0 0 32 32" width="30" height="30" aria-hidden="true">
		<path
			class="body"
			d="M2.6 15.1 12.2 5.5A3 3 0 0 1 14.3 4.6H27a2 2 0 0 1 2 2v18.8a2 2 0 0 1-2 2H14.3a3 3 0 0 1-2.1-.9l-9.6-9.6a1.3 1.3 0 0 1 0-1.8z"
		/>
		<circle class="hole" cx="11.2" cy="16" r="2.3" />
		<path class="env" d="m16.5 11.6 4.8 3.6 4.8-3.6M16.5 11.4v9.2h9.6v-9.2" />
	</svg>
	<span class="word">tag<b>mail</b></span>
</a>

<style>
	.logo {
		display: flex;
		align-items: center;
		gap: 9px;
		color: var(--ink);
		text-decoration: none;
	}

	.body {
		fill: var(--accent);
	}

	.hole {
		fill: var(--bg);
	}

	.env {
		fill: none;
		stroke: var(--accent-ink);
		stroke-width: 2;
		stroke-linecap: round;
		stroke-linejoin: round;
	}

	.word {
		font-size: 21px;
		font-weight: 520;
		letter-spacing: -0.02em;
		font-variation-settings: 'wdth' 88;
	}

	.word b {
		font-weight: 720;
	}
</style>
