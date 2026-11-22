<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { shortcuts } from '#lib/shortcuts/index.ts';
	import { lists, nav } from '#lib/stores/index.ts';
	import { api, type ThreadDetailDto } from '#lib/transport/index.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import LabelChip from '#lib/ui/LabelChip.svelte';
	import MessageCard from '#lib/ui/MessageCard.svelte';
	import { errorText } from '#lib/ui/errors.ts';
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';

	const id = $derived(Number(page.params.id));

	let detail = $state<ThreadDetailDto | null>(null);
	let error = $state<string | null>(null);
	const expanded = new SvelteSet<number>();
	let heading: HTMLHeadingElement | undefined = $state();

	const list = $derived(lists.current);
	const position = $derived.by(() => {
		void list?.indexVersion;
		return list ? list.index.indexOf(id) : -1;
	});
	const newer = $derived(position > 0 ? list?.neighbour(id, -1) : undefined);
	const older = $derived(position >= 0 ? list?.neighbour(id, 1) : undefined);

	$effect(() => {
		const current = id;
		const controller = new AbortController();
		detail = null;
		error = null;
		api.thread(current, controller.signal).then(
			(d) => {
				if (d.thread.id !== current) {
					void goto(`/thread/${d.thread.id}`, { replaceState: true });
					return;
				}
				detail = d;
				expanded.clear();
				for (const m of d.messages) if (m.unread) expanded.add(m.id);
				const last = d.messages.at(-1);
				if (last) expanded.add(last.id);
				if (list && list.index.has(current)) list.selectedId = current;
				requestAnimationFrame(() => heading?.focus({ preventScroll: true }));
			},
			(e: unknown) => {
				if (!controller.signal.aborted) error = errorText(e);
			}
		);
		return () => controller.abort();
	});

	onMount(() =>
		shortcuts.push((action) => {
			if (action === 'next' && older !== undefined) {
				void goto(`/thread/${older}`);
				return true;
			}
			if (action === 'previous' && newer !== undefined) {
				void goto(`/thread/${newer}`);
				return true;
			}
			return action === 'next' || action === 'previous';
		})
	);

	function toggle(messageId: number) {
		if (!expanded.delete(messageId)) expanded.add(messageId);
	}
</script>

<svelte:head>
	<title>{detail?.thread.subject || 'Conversation'} · {APP_NAME}</title>
</svelte:head>

<article class="thread" aria-busy={detail === null && error === null}>
	<div class="toolbar">
		<a class="icon-btn" href={lists.currentPath} aria-label="Back to the list" title="Back (u)">
			<Icon name="back" />
		</a>
		<span class="spacer"></span>
		{#if list && position >= 0}
			<span class="pos">{(position + 1).toLocaleString()} of {list.count.toLocaleString()}</span>
		{/if}
		<a
			class="icon-btn"
			href={newer !== undefined ? `/thread/${newer}` : undefined}
			aria-disabled={newer === undefined}
			aria-label="Newer conversation"
			title="Newer (k)"
		>
			<Icon name="up" />
		</a>
		<a
			class="icon-btn"
			href={older !== undefined ? `/thread/${older}` : undefined}
			aria-disabled={older === undefined}
			aria-label="Older conversation"
			title="Older (j)"
		>
			<Icon name="down" />
		</a>
	</div>

	{#if error}
		<div class="state" role="alert">
			<strong>Couldn't open this conversation</strong>
			<span>{error}</span>
		</div>
	{:else if !detail}
		<div class="state" role="status"><span class="spinner" aria-hidden="true"></span>Opening</div>
	{:else}
		<header class="head">
			<h1 bind:this={heading} tabindex="-1">{detail.thread.subject || '(no subject)'}</h1>
			<div class="chips">
				{#each detail.thread.labels as key (key)}
					<LabelChip labelKey={key} name={nav.labelName(key)} />
				{/each}
			</div>
		</header>

		<ol class="messages">
			{#each detail.messages as m (m.id)}
				<MessageCard message={m} open={expanded.has(m.id)} ontoggle={() => toggle(m.id)} />
			{/each}
		</ol>
	{/if}
</article>

<style>
	.thread {
		height: 100%;
		overflow-y: auto;
		overscroll-behavior: contain;
	}

	.toolbar {
		position: sticky;
		top: 0;
		z-index: 2;
		display: flex;
		align-items: center;
		gap: 4px;
		height: 48px;
		padding: 0 12px;
		border-bottom: 1px solid var(--line);
		background: color-mix(in oklab, var(--surface) 92%, transparent);
		backdrop-filter: blur(8px);
	}

	.spacer {
		flex: 1;
	}

	.pos {
		margin-right: 6px;
		color: var(--muted);
		font-family: var(--font-mono);
		font-size: 12px;
	}

	.icon-btn {
		display: grid;
		place-items: center;
		width: 38px;
		height: 38px;
		border-radius: 50%;
		color: var(--ink-2);
	}

	.icon-btn:hover {
		background: var(--hover);
	}

	.icon-btn[aria-disabled='true'] {
		color: var(--faint);
		pointer-events: none;
	}

	.head {
		padding: 22px 28px 8px 76px;
	}

	h1 {
		margin: 0;
		font-size: 22px;
		font-weight: 560;
		line-height: 1.25;
		letter-spacing: -0.01em;
		outline: none;
	}

	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-top: 10px;
	}

	.chips:empty {
		display: none;
	}

	.messages {
		margin: 0;
		padding: 8px 20px 40px;
		list-style: none;
	}

	.state {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 10px;
		height: 60%;
		color: var(--muted);
	}

	.state strong {
		color: var(--ink);
	}

	.spinner {
		width: 22px;
		height: 22px;
		border: 2px solid var(--line-strong);
		border-top-color: var(--accent);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
	}

	@keyframes spin {
		to {
			transform: rotate(1turn);
		}
	}

	@media (max-width: 720px) {
		.head {
			padding: 16px 16px 4px;
		}

		h1 {
			font-size: 19px;
		}

		.messages {
			padding: 4px 6px 32px;
		}
	}
</style>
