<script lang="ts">
	import { afterNavigate } from '$app/navigation';
	import { page } from '$app/state';
	import {
		listTarget,
		readStoredPath,
		splitShown,
		threadRouteId,
		type ListTarget
	} from '#lib/readingPane.ts';
	import { currentAccount, lists, prefs } from '#lib/stores/index.ts';
	import ListPane from '#lib/ui/ListPane.svelte';
	import { untrack } from 'svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();

	/** List and conversation side by side (or stacked); otherwise the routes render as they are. */
	const shown = $derived(splitShown(prefs.split, page.url));
	const routeId = $derived(threadRouteId(page.url));
	const target: ListTarget = $derived(
		listTarget(
			page.url,
			() => (lists.current ? { view: lists.current.view, path: lists.currentPath } : null),
			readStoredPath,
			currentAccount(page.url)
		)
	);
	const ready = $derived(lists.current?.status === 'ready');

	let listPane: HTMLDivElement | undefined = $state();
	let readingPane: HTMLDivElement | undefined = $state();

	// The list highlights the open conversation (once per conversation, and once the list is ready).
	$effect(() => {
		const id = routeId;
		if (!shown || id === null || !ready) return;
		untrack(() => lists.current?.selectThread(id));
	});

	// Closing the conversation (u, back, archive, …) hands focus back to the list.
	afterNavigate((navigation) => {
		if (!shown || routeId !== null) return;
		if (!navigation.from?.url.pathname.startsWith('/thread/')) return;
		const active = document.activeElement;
		if (active && active !== document.body && !readingPane?.contains(active)) return;
		listPane?.querySelector<HTMLElement>('[role="listbox"]')?.focus({ preventScroll: true });
	});
</script>

{#if shown}
	<div class="split-host">
		<div class="split" class:below={prefs.readingPane === 'below'}>
			<div class="list-pane" bind:this={listPane}>
				{#key `${target.view}|${prefs.inboxType}`}
					<ListPane {target} {routeId} />
				{/key}
			</div>
			<div class="reading-pane" bind:this={readingPane} data-sveltekit-reset="false">
				{@render children()}
				{#if routeId === null}
					<div class="empty">
						<div class="mark" aria-hidden="true"></div>
						<span>Select a conversation</span>
					</div>
				{/if}
			</div>
		</div>
	</div>
{:else}
	{@render children()}
{/if}

<style>
	.split-host {
		display: flex;
		container-type: inline-size;
	}

	.split {
		display: grid;
		flex: 1;
		grid-template-columns: minmax(320px, 40%) minmax(0, 1fr);
		grid-template-rows: minmax(0, 1fr);
		min-width: 0;
		min-height: 0;
	}

	.split.below {
		grid-template-columns: minmax(0, 1fr);
		grid-template-rows: 45% minmax(0, 1fr);
	}

	.list-pane,
	.reading-pane {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
		overflow: hidden;
	}

	.list-pane > :global(*),
	.reading-pane > :global(*) {
		flex: 1;
		min-height: 0;
	}

	.list-pane {
		border-right: 1px solid var(--line);
	}

	.below .list-pane {
		border-right: 0;
		border-bottom: 1px solid var(--line);
	}

	@container (max-width: 760px) {
		.split {
			grid-template-columns: minmax(0, 1fr);
			grid-template-rows: 45% minmax(0, 1fr);
		}

		.list-pane {
			border-right: 0;
			border-bottom: 1px solid var(--line);
		}
	}

	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 10px;
		padding: 32px;
		color: var(--muted);
		text-align: center;
	}

	.mark {
		width: 56px;
		height: 40px;
		margin-bottom: 6px;
		clip-path: polygon(14px 0, 100% 0, 100% 100%, 14px 100%, 0 50%);
		border-radius: 0 8px 8px 0;
		background: var(--accent-soft);
	}
</style>
