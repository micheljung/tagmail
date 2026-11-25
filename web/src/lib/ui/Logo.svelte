<a class="logo" href="/inbox" aria-label="Tagmail, go to Inbox">
	<svg class="point" viewBox="0 0 12 32" aria-hidden="true">
		<path
			fill-rule="evenodd"
			d="M13 0V32H11L1.3 17.6Q.2 16 1.3 14.4L11 0ZM5.8 16a2.2 2.2 0 1 0 4.4 0 2.2 2.2 0 1 0-4.4 0Z"
		/>
	</svg>
	<span class="tag">tag</span><span class="mail">mail</span>
</a>

<style>
	.logo {
		display: flex;
		align-items: center;
		height: 30px;
		color: var(--ink);
		font-size: 21px;
		font-weight: 600;
		letter-spacing: -0.025em;
		text-decoration: none;
	}

	.point {
		width: 11.25px;
		height: 30px;
		margin-right: -1px;
		fill: var(--accent);
	}

	.tag {
		display: flex;
		align-items: center;
		height: 30px;
		padding: 0 7px 1px 2px;
		border-radius: 0 6px 6px 0;
		background: var(--accent);
		color: var(--accent-ink);
	}

	.mail {
		padding-left: 3px;
	}
</style>
