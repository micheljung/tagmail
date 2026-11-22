<script lang="ts">
	import { addressList, displayName, fileSize, fullDate, listDate } from '../format';
	import { api, downloadUrl, type MessageDto, type ThreadMessageDto } from '../transport';
	import { errorText } from './errors';
	import Icon from './Icon.svelte';
	import MessageFrame from './MessageFrame.svelte';

	let {
		message,
		open,
		ontoggle
	}: { message: ThreadMessageDto; open: boolean; ontoggle: () => void } = $props();

	/** Part URLs are signed for 10 minutes; refresh a bit before that. */
	const PART_URL_TTL_MS = 9 * 60_000;

	let detail = $state<MessageDto | null>(null);
	let fetchedAt = 0;
	let error = $state<string | null>(null);
	let showImages = $state(false);
	let allowing = $state(false);

	const sender = $derived(displayName(message.from));
	const attachments = $derived(detail ? detail.parts.filter((p) => !p.inline) : []);
	const blockedImages = $derived(
		!!detail && detail.remoteImages > 0 && !detail.imagesAllowed && !showImages
	);

	async function load(): Promise<MessageDto | null> {
		try {
			detail = await api.message(message.id);
			fetchedAt = Date.now();
			error = null;
		} catch (e) {
			error = errorText(e);
		}
		return detail;
	}

	$effect(() => {
		if (open && !detail) void load();
	});

	async function allowSender() {
		const address = message.from?.email;
		if (!address) return;
		allowing = true;
		try {
			await api.setImageSender(address, true);
			showImages = true;
		} catch (e) {
			error = errorText(e);
		} finally {
			allowing = false;
		}
	}

	async function download(event: MouseEvent, partId: number) {
		if (Date.now() - fetchedAt < PART_URL_TTL_MS) return;
		event.preventDefault();
		const fresh = await load();
		const part = fresh?.parts.find((p) => p.id === partId);
		if (part) location.assign(downloadUrl(part.url));
	}
</script>

<li class="message" class:open class:unread={message.unread}>
	<button
		class="summary"
		aria-expanded={open}
		aria-controls="message-{message.id}"
		onclick={ontoggle}
	>
		<span class="avatar" aria-hidden="true">{sender.slice(0, 1).toUpperCase()}</span>
		<span class="who">
			<strong>{sender}</strong>
			{#if open && message.from}<span class="addr">&lt;{message.from.email}&gt;</span>{/if}
		</span>
		{#if !open}<span class="snip">{message.snippet}</span>{/if}
		<span class="when">
			{#if message.hasAttachment}<Icon name="clip" size={15} label="Has attachment" />{/if}
			<time datetime={new Date(message.date * 1000).toISOString()} title={fullDate(message.date)}>
				{open ? fullDate(message.date) : listDate(message.date)}
			</time>
			{#if message.starred}<span class="star">
					<Icon name="star" size={16} filled label="Starred" />
				</span>{/if}
		</span>
	</button>
	{#if open}
		<div class="content" id="message-{message.id}">
			<dl class="recipients">
				{#if message.to.length}<div>
						<dt>to</dt>
						<dd>{addressList(message.to)}</dd>
					</div>{/if}
				{#if message.cc.length}<div>
						<dt>cc</dt>
						<dd>{addressList(message.cc)}</dd>
					</div>{/if}
			</dl>
			{#if error}<p class="error" role="alert">{error}</p>{/if}
			{#if blockedImages}
				<div class="images" role="note">
					<Icon name="shield" size={18} />
					<span>Remote images are blocked so the sender can't see when you read this.</span>
					<button class="btn" onclick={() => (showImages = true)}>
						<Icon name="image" size={16} />Show images
					</button>
					{#if message.from}
						<button class="link" onclick={allowSender} disabled={allowing}>
							Always show from {message.from.email}
						</button>
					{/if}
				</div>
			{/if}
			{#if detail}
				<MessageFrame
					messageId={message.id}
					images={showImages || detail.imagesAllowed}
					title={`Message from ${sender}`}
				/>
				{#if attachments.length > 0}
					<ul class="files" aria-label="Attachments">
						{#each attachments as part (part.id)}
							<li>
								<a
									href={downloadUrl(part.url)}
									onclick={(e) => download(e, part.id)}
									download={part.name ?? ''}
								>
									<Icon name="clip" size={16} />
									<span class="name">{part.name ?? part.contentType}</span>
									<span class="size">{fileSize(part.size)}</span>
								</a>
							</li>
						{/each}
					</ul>
				{/if}
			{:else if !error}
				<div class="placeholder" aria-hidden="true"></div>
			{/if}
		</div>
	{/if}
</li>

<style>
	.message {
		border-bottom: 1px solid var(--line);
	}

	.message:last-child {
		border-bottom: 0;
	}

	.summary {
		display: grid;
		grid-template-columns: 40px minmax(0, max-content) minmax(0, 1fr) auto;
		align-items: center;
		gap: 12px;
		width: 100%;
		padding: 12px 8px;
		border: 0;
		border-radius: 10px;
		background: none;
		text-align: left;
		cursor: pointer;
	}

	.summary:hover {
		background: var(--hover);
	}

	.open .summary {
		grid-template-columns: 40px minmax(0, 1fr) auto;
	}

	.avatar {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border-radius: 50%;
		background: var(--accent-soft);
		color: var(--accent);
		font-weight: 700;
	}

	.who {
		display: flex;
		align-items: baseline;
		gap: 6px;
		min-width: 0;
		white-space: nowrap;
	}

	.who strong {
		font-weight: 640;
	}

	.unread .who strong {
		font-weight: 760;
	}

	.addr {
		overflow: hidden;
		color: var(--muted);
		font-size: 12.5px;
		text-overflow: ellipsis;
	}

	.snip {
		overflow: hidden;
		color: var(--muted);
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.when {
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--muted);
		font-family: var(--font-mono);
		font-size: 12px;
		white-space: nowrap;
	}

	.star {
		color: var(--star);
	}

	.content {
		padding: 0 8px 20px 60px;
	}

	.recipients {
		margin: -6px 0 12px;
		color: var(--muted);
		font-size: 12.5px;
	}

	.recipients div {
		display: flex;
		gap: 6px;
	}

	.recipients dd {
		margin: 0;
	}

	.images {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 10px;
		margin-bottom: 12px;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: 10px;
		background: var(--surface-2);
		color: var(--ink-2);
		font-size: 13px;
	}

	.images span {
		flex: 1;
		min-width: 200px;
	}

	.link {
		border: 0;
		background: none;
		color: var(--accent);
		font-size: 13px;
		cursor: pointer;
	}

	.link:hover {
		text-decoration: underline;
	}

	.error {
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
	}

	.placeholder {
		height: 120px;
		border-radius: 8px;
		background: linear-gradient(90deg, var(--line) 0%, var(--hover) 50%, var(--line) 100%);
		background-size: 200% 100%;
		animation: shimmer 1.2s linear infinite;
		opacity: 0.6;
	}

	@keyframes shimmer {
		to {
			background-position: -200% 0;
		}
	}

	.files {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		margin: 14px 0 0;
		padding: 0;
		list-style: none;
	}

	.files a {
		display: flex;
		align-items: center;
		gap: 8px;
		max-width: 280px;
		padding: 8px 12px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		color: var(--ink-2);
		text-decoration: none;
	}

	.files a:hover {
		background: var(--hover);
	}

	.name {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.size {
		color: var(--muted);
		font-family: var(--font-mono);
		font-size: 11.5px;
	}

	@media (max-width: 720px) {
		.summary {
			grid-template-columns: 36px minmax(0, 1fr) auto;
		}

		.snip {
			display: none;
		}

		.content {
			padding: 0 6px 16px;
		}

		.when time {
			font-size: 11px;
		}
	}
</style>
