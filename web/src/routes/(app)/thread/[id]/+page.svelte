<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import ComposeForm from '#lib/compose/ComposeForm.svelte';
	import { composer } from '#lib/compose/composer.svelte.ts';
	import type { Draft } from '#lib/compose/draft.svelte.ts';
	import { act } from '#lib/ops/actions.svelte.ts';
	import { patchRow } from '#lib/ops/effects.ts';
	import { shortcuts, type ShortcutAction } from '#lib/shortcuts/index.ts';
	import { lists, nav, ops, toasts } from '#lib/stores/index.ts';
	import { api, type ReplyMode, type ThreadDetailDto } from '#lib/transport/index.ts';
	import type { IconName } from '#lib/ui/Icon.svelte';
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
	let reply = $state.raw<Draft | null>(null);
	let replying = $state<ReplyMode | null>(null);
	let replyForm: ComposeForm | undefined = $state();

	const thread = $derived(detail ? ops.project(detail.thread) : null);

	/** Actions that take the conversation out of view send the player back to the list. */
	const LEAVES: ReadonlySet<ShortcutAction> = new Set(['archive', 'delete', 'markUnread']);

	const TOOLBAR: { action: ShortcutAction; icon: IconName; label: string; key: string }[] = [
		{ action: 'archive', icon: 'archive', label: 'Archive', key: 'e' },
		{ action: 'delete', icon: 'trash', label: 'Delete', key: '#' },
		{ action: 'markUnread', icon: 'unread', label: 'Mark as unread', key: 'Shift+U' },
		{ action: 'label', icon: 'tag', label: 'Label as', key: 'l' },
		{ action: 'move', icon: 'folder', label: 'Move to', key: 'v' }
	];

	function run(action: ShortcutAction): boolean {
		if (!thread) return false;
		const view = list && list.index.has(thread.id) ? list.view : null;
		const done = act(action, { threadIds: [thread.id], rows: [thread], view });
		if (done && LEAVES.has(action)) void goto(lists.currentPath);
		return done;
	}

	async function startReply(mode: ReplyMode) {
		const last = detail?.messages.at(-1);
		if (!last || replying) return;
		if (reply) await composer.close(reply);
		replying = mode;
		try {
			reply = await composer.reply(last.id, mode);
			requestAnimationFrame(() => {
				replyForm?.focus();
				document.getElementById('reply')?.scrollIntoView({ block: 'nearest' });
			});
		} catch (e) {
			toasts.show(`Couldn't start the reply: ${errorText(e)}`, { tone: 'error' });
		} finally {
			replying = null;
		}
	}

	async function sendReply() {
		if (!reply) return;
		if (await composer.send(reply)) reply = null;
	}

	async function discardReply() {
		const d = reply;
		reply = null;
		if (d) await composer.close(d, true);
	}

	$effect(() =>
		ops.onSettled((op) => {
			if (detail && op.intent.threadIds.includes(detail.thread.id)) {
				detail = { ...detail, thread: patchRow(detail.thread, op.intent) };
			}
		})
	);

	$effect(() => {
		void id;
		return () => {
			const d = reply;
			reply = null;
			if (d) void composer.close(d);
		};
	});

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
					void goto(`/thread/${d.thread.id}`, { replace: true, state: page.state });
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
			if (action === 'reply' || action === 'replyAll' || action === 'forward') {
				void startReply(action === 'reply' ? 'reply' : action === 'replyAll' ? 'all' : 'forward');
				return true;
			}
			if (action === 'next' || action === 'previous') return true;
			return run(action);
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
		<a
			class="icon-btn back"
			href={lists.currentPath}
			aria-label="Back to the list"
			title="Back (u)"
		>
			<Icon name="back" />
		</a>
		{#if thread}
			<span class="sep" aria-hidden="true"></span>
			{#each TOOLBAR as item (item.action)}
				<button
					class="icon-btn"
					aria-label={item.label}
					title="{item.label} ({item.key})"
					onclick={() => run(item.action)}
				>
					<Icon name={item.icon} />
				</button>
			{/each}
		{/if}
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
			<div class="title">
				<h1 bind:this={heading} tabindex="-1">{detail.thread.subject || '(no subject)'}</h1>
				{#if thread}
					<button
						class="icon-btn star"
						class:on={thread.starred}
						aria-pressed={thread.starred}
						aria-label={thread.starred ? 'Starred' : 'Not starred'}
						title="Star (s)"
						onclick={() => run('star')}
					>
						<Icon name="star" filled={thread.starred} />
					</button>
				{/if}
			</div>
			<div class="chips" aria-label="Labels">
				{#each thread?.labels ?? [] as key (key)}
					<LabelChip labelKey={key} name={nav.labelName(key)} color={nav.labelColor(key)} />
				{/each}
			</div>
		</header>

		<ol class="messages">
			{#each detail.messages as m (m.id)}
				<MessageCard message={m} open={expanded.has(m.id)} ontoggle={() => toggle(m.id)} />
			{/each}
		</ol>

		<section class="reply" id="reply" aria-label="Reply">
			{#if reply}
				<div class="reply-head">
					<Icon
						name={reply.mode === 'forward'
							? 'forward'
							: reply.mode === 'all'
								? 'replyAll'
								: 'reply'}
						size={18}
					/>
					<span>
						{reply.mode === 'forward' ? 'Forward' : reply.mode === 'all' ? 'Reply all' : 'Reply'}
					</span>
				</div>
				<ComposeForm
					bind:this={replyForm}
					draft={reply}
					inline
					onsend={() => void sendReply()}
					ondiscard={() => void discardReply()}
				/>
			{:else}
				<div class="reply-actions">
					<button class="btn" onclick={() => startReply('reply')} disabled={replying !== null}>
						<Icon name="reply" size={17} />Reply
					</button>
					<button class="btn" onclick={() => startReply('all')} disabled={replying !== null}>
						<Icon name="replyAll" size={17} />Reply all
					</button>
					<button class="btn" onclick={() => startReply('forward')} disabled={replying !== null}>
						<Icon name="forward" size={17} />Forward
					</button>
				</div>
			{/if}
		</section>
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

	.sep {
		width: 1px;
		height: 22px;
		margin: 0 6px;
		background: var(--line);
	}

	button.icon-btn {
		border: 0;
		background: none;
		cursor: pointer;
	}

	.title {
		display: flex;
		align-items: flex-start;
		gap: 8px;
	}

	.title h1 {
		flex: 1;
	}

	.star {
		margin-top: -6px;
		color: var(--faint);
	}

	.star.on {
		color: var(--star);
	}

	.reply {
		padding: 0 20px 48px 76px;
	}

	.reply-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
	}

	.reply-head {
		display: flex;
		align-items: center;
		gap: 8px;
		margin-bottom: 8px;
		color: var(--muted);
		font-weight: 600;
	}

	.reply :global(.form.inline) {
		min-height: 320px;
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
		padding: 8px 20px 16px;
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

	@media (pointer: coarse), (max-width: 720px) {
		.icon-btn {
			width: 44px;
			height: 44px;
		}

		.toolbar {
			gap: 0;
			padding: 0 4px;
		}
	}

	@media (max-width: 720px) {
		.back,
		.pos {
			display: none;
		}

		.head {
			padding: 16px 16px 4px;
		}

		h1 {
			font-size: 19px;
		}

		.messages {
			padding: 4px 6px 32px;
		}

		.reply {
			padding: 0 8px 32px;
		}

		.sep {
			display: none;
		}
	}
</style>
