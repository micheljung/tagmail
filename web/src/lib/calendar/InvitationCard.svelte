<script lang="ts">
	import { ApiError, api } from '../transport';
	import { errorText } from '../ui/errors';
	import Icon from '../ui/Icon.svelte';
	import {
		ATTENDEE_COLLAPSE_LIMIT,
		eventWhen,
		isHttpUrl,
		linkify,
		recurrenceSummary,
		senderWarning,
		visibleAttendees
	} from './invitation';
	import type { InvitationAttendeeDto, InvitationDto, InvitationEventDto } from './types';

	let { messageId }: { messageId: number } = $props();

	let invitation = $state<InvitationDto | null>(null);
	let error = $state<string | null>(null);
	let expanded = $state<Record<number, boolean>>({});

	$effect(() => {
		const controller = new AbortController();
		invitation = null;
		error = null;
		api
			.invitation(messageId, controller.signal)
			.then((dto) => (invitation = dto))
			.catch((e: unknown) => {
				if (!controller.signal.aborted) error = failureText(e);
			});
		return () => controller.abort();
	});

	const SILENT_CODES = new Set(['no_invitation', 'feature_disabled']);
	const FAILURES: Record<string, string> = {
		unparsable: "Couldn't read this invitation.",
		body_unavailable: 'The invitation is not available right now.'
	};

	function failureText(e: unknown): string | null {
		if (e instanceof ApiError) {
			if (SILENT_CODES.has(e.code)) return null;
			return FAILURES[e.code] ?? errorText(e);
		}
		return errorText(e);
	}

	const warning = $derived(
		invitation ? senderWarning(invitation.method, invitation.senderAuth) : null
	);
	const cancelled = (event: InvitationEventDto) =>
		event.cancelled || invitation?.method === 'cancel';
	const person = (p: Pick<InvitationAttendeeDto, 'name' | 'email'>) => p.name?.trim() || p.email;
</script>

{#if error}
	<p class="note error" role="alert">{error}</p>
{:else if invitation}
	{#if invitation.tooLarge}
		<p class="note" role="note"><Icon name="alert" size={16} />Invitation too large to show.</p>
	{/if}
	{#each invitation.events as event, index (index)}
		{@const when = eventWhen(event)}
		{@const repeat = recurrenceSummary(event.recurrence)}
		{@const people = visibleAttendees(event.attendees, !!expanded[index])}
		<section class="card" class:cancelled={cancelled(event)} aria-label="Calendar invitation">
			{#if invitation.outdated}
				<p class="banner warn" role="status">
					<Icon name="alert" size={16} />Outdated invitation
				</p>
			{/if}
			{#if cancelled(event)}
				<p class="banner cancel" role="status">
					<Icon name="close" size={16} />This event was cancelled.
				</p>
			{/if}
			{#if warning}
				<p class="banner warn" role="alert"><Icon name="alert" size={16} />{warning}</p>
			{/if}
			<div class="head">
				<span class="badge" aria-hidden="true"><Icon name="calendar" size={20} /></span>
				<div class="titles">
					<h2>{event.summary?.trim() || 'Untitled event'}</h2>
					<p class="when">
						<time datetime={event.start}>{when.primary}</time>
					</p>
					{#if when.original}<p class="original">Organizer's time: {when.original}</p>{/if}
					{#if repeat}<p class="repeat">{repeat}</p>{/if}
				</div>
			</div>
			<dl class="facts">
				{#if event.location}
					<div>
						<dt>Where</dt>
						<dd>{@render linked(event.location)}</dd>
					</div>
				{/if}
				{#if event.organizer}
					<div>
						<dt>Organizer</dt>
						<dd>
							{person(event.organizer)}
							{#if event.organizer.name}<span class="addr">
									&lt;{event.organizer.email}&gt;
								</span>{/if}
							{#if event.organizer.altEmail}<span class="addr">
									also listed as {event.organizer.altEmail}
								</span>{/if}
						</dd>
					</div>
				{/if}
				{#if event.attendees.length}
					<div>
						<dt>Attendees</dt>
						<dd>
							<ul class="people">
								{#each people as a, i (i)}
									<li>
										{person(a)}{#if a.partstat !== 'needs_action' && a.partstat !== 'other'}
											<span class="status">{a.partstat}</span>{/if}
									</li>
								{/each}
							</ul>
							{#if event.attendees.length > ATTENDEE_COLLAPSE_LIMIT}
								<button
									class="link"
									aria-expanded={!!expanded[index]}
									onclick={() => (expanded[index] = !expanded[index])}
								>
									{expanded[index] ? 'Show fewer' : `Show all ${event.attendees.length} attendees`}
								</button>
							{/if}
						</dd>
					</div>
				{/if}
				{#if event.url}
					<div>
						<dt>Link</dt>
						<dd>{@render linked(event.url)}</dd>
					</div>
				{/if}
			</dl>
			{#if event.description}
				<p class="description">{@render linked(event.description)}</p>
			{/if}
			{#if !cancelled(event)}
				<div class="actions">
					{#if invitation.method === 'request'}
						<button class="btn primary" disabled>Accept</button>
						<button class="btn" disabled>Tentative</button>
						<button class="btn danger" disabled>Decline</button>
					{:else if invitation.method === 'publish' || invitation.method === 'none'}
						<button class="btn primary" disabled>Add to calendar</button>
					{/if}
					{#if invitation.target === null && (invitation.method === 'request' || invitation.method === 'publish' || invitation.method === 'none')}
						<span class="hint">Calendar not set up</span>
					{/if}
				</div>
			{/if}
		</section>
	{/each}
	{#if invitation.truncated}
		<p class="note" role="note">
			<Icon name="alert" size={16} />This message has more invitations than are shown.
		</p>
	{/if}
{/if}

{#snippet linked(text: string)}
	{#each linkify(text) as segment, i (i)}
		{#if segment.href && isHttpUrl(segment.href)}
			<a href={segment.href} target="_blank" rel="noopener noreferrer">{segment.text}</a>
		{:else}
			{segment.text}
		{/if}
	{/each}
{/snippet}

<style>
	.card {
		margin: 0 0 14px;
		border: 1px solid var(--line-strong);
		border-left: 4px solid var(--accent);
		border-radius: var(--radius);
		background: var(--surface-2);
		overflow: hidden;
	}

	.card.cancelled {
		border-left-color: var(--danger);
	}

	.card.cancelled h2 {
		text-decoration: line-through;
		color: var(--muted);
	}

	.banner {
		display: flex;
		align-items: center;
		gap: 8px;
		margin: 0;
		padding: 8px 14px;
		font-size: 13px;
		font-weight: 560;
	}

	.banner.cancel {
		background: var(--danger-soft);
		color: var(--danger);
	}

	.banner.warn {
		border-bottom: 1px solid var(--line);
		background: color-mix(in srgb, var(--star) 16%, transparent);
		color: var(--ink);
	}

	.banner.warn :global(svg) {
		color: var(--star);
	}

	.head {
		display: flex;
		gap: 12px;
		padding: 14px 14px 6px;
	}

	.badge {
		display: grid;
		flex: none;
		place-items: center;
		width: 40px;
		height: 40px;
		border-radius: 10px;
		background: var(--accent-soft);
		color: var(--accent);
	}

	.titles {
		min-width: 0;
	}

	h2 {
		margin: 0;
		font-size: 17px;
		font-weight: 680;
		line-height: 1.25;
		overflow-wrap: anywhere;
	}

	.titles p {
		margin: 3px 0 0;
	}

	.when {
		font-weight: 560;
	}

	.original,
	.repeat {
		color: var(--muted);
		font-size: 12.5px;
	}

	.facts {
		display: grid;
		gap: 6px;
		margin: 0;
		padding: 8px 14px 12px 66px;
	}

	.facts div {
		display: grid;
		grid-template-columns: 84px minmax(0, 1fr);
		gap: 10px;
	}

	dt {
		color: var(--muted);
		font-size: 12.5px;
	}

	dd {
		margin: 0;
		overflow-wrap: anywhere;
	}

	.addr,
	.status {
		color: var(--muted);
		font-size: 12.5px;
	}

	.status {
		margin-left: 6px;
		text-transform: capitalize;
	}

	.people {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}

	.description {
		margin: 0;
		padding: 0 14px 12px 66px;
		color: var(--ink-2);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}

	a {
		color: var(--accent);
	}

	.link {
		margin-top: 4px;
		padding: 0;
		border: 0;
		background: none;
		color: var(--accent);
		font-size: 13px;
		cursor: pointer;
	}

	.link:hover {
		text-decoration: underline;
	}

	.actions {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 8px;
		padding: 10px 14px;
		border-top: 1px solid var(--line);
	}

	.hint {
		margin-left: 4px;
		color: var(--muted);
		font-size: 12.5px;
	}

	.note {
		display: flex;
		align-items: center;
		gap: 8px;
		margin: 0 0 12px;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--radius);
		background: var(--surface-2);
		color: var(--ink-2);
		font-size: 13px;
	}

	.note.error {
		border-color: transparent;
		background: var(--danger-soft);
		color: var(--danger);
	}

	@media (max-width: 720px) {
		.facts,
		.description {
			padding-left: 14px;
		}

		.facts div {
			grid-template-columns: 1fr;
			gap: 0;
		}
	}
</style>
