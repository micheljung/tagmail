<script lang="ts">
	import { SvelteSet } from 'svelte/reactivity';
	import Icon from '../ui/Icon.svelte';
	import ComposeForm from './ComposeForm.svelte';
	import { composer } from './composer.svelte';
	import type { Draft } from './draft.svelte';

	const minimized = new SvelteSet<Draft>();
	const maximized = new SvelteSet<Draft>();

	function title(d: Draft): string {
		return d.subject.trim() || 'New message';
	}

	function toggle(set: SvelteSet<Draft>, d: Draft) {
		if (!set.delete(d)) set.add(d);
	}

	/** A minimized window is out of the way like a closed one, so it goes to Drafts now. */
	function minimize(d: Draft) {
		toggle(minimized, d);
		if (minimized.has(d)) void d.save({ imap: true }).catch(() => undefined);
	}
</script>

<div class="dock">
	{#each composer.windows as draft (draft)}
		{@const min = minimized.has(draft)}
		<section
			class="window"
			class:min
			class:max={maximized.has(draft)}
			aria-label="Compose: {title(draft)}"
			{@attach (node) => {
				const onkeydown = (e: KeyboardEvent) => {
					if (e.key === 'Escape' && !e.defaultPrevented) {
						e.preventDefault();
						void composer.close(draft);
					}
				};
				node.addEventListener('keydown', onkeydown);
				return () => node.removeEventListener('keydown', onkeydown);
			}}
		>
			<header>
				<button class="title" onclick={() => minimize(draft)} aria-expanded={!min}>
					{title(draft)}
				</button>
				<button
					class="ctl"
					aria-label={min ? 'Restore' : 'Minimize'}
					title={min ? 'Restore' : 'Minimize'}
					onclick={() => minimize(draft)}
				>
					<Icon name={min ? 'up' : 'minimize'} size={17} />
				</button>
				<button
					class="ctl wide"
					aria-label="Full screen"
					title="Full screen"
					onclick={() => {
						minimized.delete(draft);
						toggle(maximized, draft);
					}}
				>
					<Icon name="expand" size={15} />
				</button>
				<button
					class="ctl"
					aria-label="Save and close"
					title="Save and close (Esc)"
					onclick={() => void composer.close(draft)}
				>
					<Icon name="close" size={17} />
				</button>
			</header>
			<div class="body" hidden={min}>
				<ComposeForm
					{draft}
					onsend={() => void composer.send(draft)}
					ondiscard={() => void composer.close(draft, true)}
				/>
			</div>
		</section>
	{/each}
</div>

<style>
	.dock {
		position: fixed;
		right: 24px;
		bottom: 0;
		z-index: 60;
		display: flex;
		flex-direction: row-reverse;
		align-items: flex-end;
		gap: 14px;
		pointer-events: none;
	}

	.window {
		display: flex;
		flex-direction: column;
		width: 540px;
		height: min(600px, calc(100dvh - 90px));
		overflow: hidden;
		border: 1px solid var(--line-strong);
		border-bottom: 0;
		border-radius: 14px 14px 0 0;
		background: var(--surface);
		box-shadow:
			0 -2px 6px rgb(20 23 28 / 0.06),
			0 18px 60px rgb(20 23 28 / 0.28);
		pointer-events: auto;
		animation: slide 200ms var(--ease);
	}

	@keyframes slide {
		from {
			transform: translateY(40px);
			opacity: 0;
		}
	}

	.window.min {
		width: 300px;
		height: auto;
	}

	.window.max {
		position: fixed;
		inset: 5vh 8vw;
		width: auto;
		height: auto;
		border-bottom: 1px solid var(--line-strong);
		border-radius: 16px;
	}

	header {
		display: flex;
		align-items: center;
		gap: 2px;
		height: 42px;
		padding: 0 6px 0 4px;
		background: #1d2128;
		color: #eef1f5;
	}

	.title {
		flex: 1;
		min-width: 0;
		height: 42px;
		padding: 0 12px;
		overflow: hidden;
		border: 0;
		background: none;
		color: inherit;
		font-weight: 620;
		text-align: left;
		white-space: nowrap;
		text-overflow: ellipsis;
		cursor: pointer;
	}

	.ctl {
		display: grid;
		place-items: center;
		width: 30px;
		height: 30px;
		border: 0;
		border-radius: 7px;
		background: none;
		color: rgb(238 241 245 / 0.75);
		cursor: pointer;
	}

	.ctl:hover {
		background: rgb(255 255 255 / 0.1);
		color: #fff;
	}

	.body {
		display: flex;
		flex: 1;
		min-height: 0;
	}

	.body[hidden] {
		display: none;
	}

	@media (pointer: coarse), (max-width: 720px) {
		header,
		.title {
			height: 48px;
		}

		.ctl {
			width: 44px;
			height: 44px;
		}
	}

	@media (max-width: 720px) {
		.dock {
			inset: 0;
			right: 0;
			display: block;
		}

		.window,
		.window.max {
			position: fixed;
			inset: 0;
			width: auto;
			height: 100dvh;
			padding-bottom: env(safe-area-inset-bottom);
			border: 0;
			border-radius: 0;
		}

		.window:not(.min) header {
			height: calc(48px + env(safe-area-inset-top));
			padding-top: env(safe-area-inset-top);
		}

		.window.min {
			inset: auto 12px 0 auto;
			width: 240px;
			height: auto;
			border-radius: 12px 12px 0 0;
		}

		.wide {
			display: none;
		}
	}
</style>
