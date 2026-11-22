<script lang="ts">
	import type { Snippet } from 'svelte';
	import Logo from './Logo.svelte';

	let { title, lead, children }: { title: string; lead: string; children: Snippet } = $props();
</script>

<div class="page">
	<div class="card">
		<Logo />
		<h1>{title}</h1>
		<p class="lead">{lead}</p>
		{@render children()}
	</div>
	<div class="art" aria-hidden="true">
		{#each Array.from({ length: 7 }, (_, i) => i) as i (i)}
			<span class="t t{i}"></span>
		{/each}
	</div>
</div>

<style>
	.page {
		position: relative;
		display: grid;
		place-items: center;
		min-height: 100dvh;
		padding: 24px 16px;
		overflow: hidden;
	}

	.card {
		position: relative;
		z-index: 1;
		width: min(400px, 100%);
		padding: 32px 32px 28px;
		border: 1px solid var(--line);
		border-radius: 22px;
		background: var(--surface);
		box-shadow: var(--shadow-lg);
	}

	h1 {
		margin: 28px 0 6px;
		font-size: 24px;
		font-weight: 620;
		letter-spacing: -0.015em;
	}

	.lead {
		margin: 0 0 22px;
		color: var(--muted);
	}

	.card :global(label) {
		display: grid;
		gap: 6px;
		margin-bottom: 14px;
		font-size: 13px;
		font-weight: 600;
		color: var(--ink-2);
	}

	.card :global(input) {
		height: 44px;
		padding: 0 14px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface-2);
		font-size: 15px;
		font-weight: 400;
	}

	.card :global(input:focus) {
		border-color: var(--accent);
		outline: 3px solid var(--accent-soft);
		outline-offset: 0;
	}

	.card :global(.btn.primary) {
		justify-content: center;
		width: 100%;
		height: 44px;
		margin-top: 6px;
	}

	.card :global(.error) {
		margin: 0 0 14px;
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
		font-size: 13px;
	}

	.card :global(.alt) {
		margin: 18px 0 0;
		color: var(--muted);
		font-size: 13px;
		text-align: center;
	}

	.art {
		position: absolute;
		inset: 0;
		pointer-events: none;
	}

	.t {
		position: absolute;
		width: 180px;
		height: 64px;
		clip-path: polygon(24px 0, 100% 0, 100% 100%, 24px 100%, 0 50%);
		border-radius: 0 14px 14px 0;
		background: oklch(var(--chip-l) var(--chip-c) var(--h));
		opacity: 0.75;
		animation: drift 18s ease-in-out infinite alternate;
	}

	.t0 {
		--h: 260;
		left: 6%;
		top: 12%;
		rotate: -14deg;
	}
	.t1 {
		--h: 150;
		left: 70%;
		top: 8%;
		rotate: 9deg;
		width: 140px;
	}
	.t2 {
		--h: 30;
		left: 78%;
		top: 62%;
		rotate: -22deg;
	}
	.t3 {
		--h: 320;
		left: 10%;
		top: 70%;
		rotate: 18deg;
		width: 120px;
	}
	.t4 {
		--h: 200;
		left: 40%;
		top: 86%;
		rotate: -6deg;
		width: 100px;
		height: 40px;
	}
	.t5 {
		--h: 85;
		left: 46%;
		top: 2%;
		rotate: 4deg;
		width: 90px;
		height: 36px;
	}
	.t6 {
		--h: 0;
		left: -2%;
		top: 42%;
		rotate: 30deg;
		width: 110px;
		height: 44px;
	}

	@keyframes drift {
		to {
			translate: 0 -18px;
		}
	}

	@media (max-width: 560px) {
		.card {
			padding: 26px 22px 22px;
		}

		.t {
			opacity: 0.4;
		}
	}
</style>
