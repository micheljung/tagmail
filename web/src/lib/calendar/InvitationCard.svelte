<script lang="ts">
	import { ops } from '../stores';
	import { ApiError, api, type CalendarDto, type OpDto, type RsvpPartstatDto } from '../transport';
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
	import { calendarNote } from './opState';
	import type {
		InvitationAttendeeDto,
		InvitationDto,
		InvitationEventDto,
		PartstatDto
	} from './types';

	let { messageId, accountId }: { messageId: number; accountId: number } = $props();

	let invitation = $state<InvitationDto | null>(null);
	let error = $state<string | null>(null);
	let expanded = $state<Record<number, boolean>>({});
	let calendars = $state<CalendarDto[]>([]);
	let selectedKey = $state<string | null>(null);
	let choice = $state<RsvpPartstatDto | null>(null);
	let reverting = $state(false);
	let replyOnly = $state(false);
	let busy = $state(false);
	let actionError = $state<string | null>(null);
	let takeover = $state<{ label: string; run: () => void } | null>(null);
	let takeoverDialog = $state<HTMLDialogElement>();
	const takeoverTitle = $props.id();

	const RSVP_ACTIONS: { partstat: RsvpPartstatDto; label: string; style: string }[] = [
		{ partstat: 'accepted', label: 'Accept', style: 'primary' },
		{ partstat: 'tentative', label: 'Tentative', style: '' },
		{ partstat: 'declined', label: 'Decline', style: 'danger' }
	];

	const ANSWERS: Partial<Record<PartstatDto, string>> = {
		accepted: 'You accepted',
		tentative: 'You tentatively accepted',
		declined: 'You declined'
	};

	const calendarKey = (c: Pick<CalendarDto, 'accountId' | 'href'>) => `${c.accountId}\n${c.href}`;

	async function fetchInvitation(signal?: AbortSignal): Promise<void> {
		try {
			invitation = await api.invitation(messageId, signal);
		} catch (e) {
			if (!signal?.aborted) error = failureText(e);
		}
	}

	$effect(() => {
		const controller = new AbortController();
		invitation = null;
		error = null;
		choice = null;
		replyOnly = false;
		actionError = null;
		void fetchInvitation(controller.signal);
		return () => controller.abort();
	});

	$effect(() => {
		if (invitation?.method !== 'publish' && invitation?.method !== 'none') return;
		const controller = new AbortController();
		api
			.calendars(controller.signal)
			.then((list) => (calendars = list))
			.catch(() => (calendars = []));
		return () => controller.abort();
	});

	$effect(() =>
		ops.onCalendarDone((op) => {
			if (!invitation?.events.some((e) => e.uid === op.calendar?.uid)) return;
			if (op.intent.kind === 'cal.rsvp') replyOnly = op.calendar?.calendarHref === null;
			else choice = null;
			void fetchInvitation();
		})
	);

	const writable = $derived(calendars.filter((c) => c.writable));
	const destination = $derived.by(() => {
		const target = invitation?.target;
		if (selectedKey) return writable.find((c) => calendarKey(c) === selectedKey);
		if (target) {
			return writable.find(
				(c) => c.accountId === target.accountId && c.href === target.calendarHref
			);
		}
		return writable[0];
	});
	const canImport = $derived(!!invitation?.target || !!destination);

	function importRequest(opId: string): Parameters<typeof api.importInvitation>[0] {
		const explicit = selectedKey || !invitation?.target ? destination : undefined;
		return {
			opId,
			messageId: String(messageId),
			...(explicit ? { accountId: explicit.accountId, calendarHref: explicit.href } : {})
		};
	}

	async function submit(send: (opId: string) => Promise<OpDto>): Promise<void> {
		busy = true;
		actionError = null;
		replyOnly = false;
		try {
			ops.track(await send(ops.newOpId()));
		} catch (e) {
			choice = null;
			actionError = errorText(e);
		} finally {
			busy = false;
		}
	}

	const allowance = () => (invitation?.organizerChanged ? { allowOrganizerChange: true } : {});

	function confirmed(label: string, run: () => void): void {
		if (invitation?.organizerChanged) takeover = { label, run };
		else run();
	}

	function reply(partstat: RsvpPartstatDto): void {
		confirmed('Send reply', () => {
			choice = partstat;
			reverting = false;
			void submit((opId) =>
				api.rsvp({ opId, messageId: String(messageId), partstat, ...allowance() })
			);
		});
	}

	function revert(partstat: RsvpPartstatDto): void {
		confirmed('Revert', () => {
			choice = null;
			reverting = true;
			void submit((opId) =>
				api.rsvp({
					opId,
					messageId: String(messageId),
					partstat,
					sendReply: false,
					...allowance()
				})
			);
		});
	}

	const addToCalendar = () => void submit((opId) => api.importInvitation(importRequest(opId)));

	const updateCalendar = () =>
		confirmed('Update calendar', () => {
			void submit((opId) =>
				api.applyInvitation({ opId, messageId: String(messageId), ...allowance() })
			);
		});

	function proceed(): void {
		const run = takeover?.run;
		takeover = null;
		run?.();
	}

	$effect(() => {
		if (!takeoverDialog) return;
		if (takeover && !takeoverDialog.open) takeoverDialog.showModal();
		else if (!takeover && takeoverDialog.open) takeoverDialog.close();
	});

	const resendable = (p: PartstatDto | null | undefined): p is RsvpPartstatDto =>
		p === 'accepted' || p === 'tentative' || p === 'declined';
	const revertible = (p: PartstatDto | null | undefined): p is RsvpPartstatDto =>
		p === 'needs_action' || resendable(p);

	const PENDING_TEXT: Record<string, string> = {
		'cal.rsvp': 'Sending reply…',
		'cal.import': 'Adding to calendar…',
		'cal.apply': 'Updating calendar…'
	};

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
		{@const op = ops.calendarOp(event.uid)}
		{@const working = busy || (!!op && op.status !== 'failed')}
		{@const note = op?.calendar ? calendarNote(op.status, op.calendar) : null}
		{@const answer = working || replyOnly ? choice : (invitation.stored?.partstat ?? null)}
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
			{#if invitation.organizerChanged}
				<p class="banner warn" role="alert">
					<Icon name="alert" size={16} />The organizer of this event changed — check before updating
				</p>
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
			{#if !cancelled(event) || invitation.autoApply !== 'none'}
				<div class="actions">
					{#if invitation.method === 'request' && !cancelled(event)}
						{#each RSVP_ACTIONS as action (action.partstat)}
							<button
								class="btn {action.style}"
								class:active={answer === action.partstat}
								aria-pressed={answer === action.partstat}
								disabled={working}
								onclick={() => reply(action.partstat)}
							>
								{action.label}
							</button>
						{/each}
					{:else if (invitation.method === 'publish' || invitation.method === 'none') && !cancelled(event)}
						{#if writable.length > 1}
							<select
								class="picker"
								aria-label="Calendar"
								value={destination ? calendarKey(destination) : ''}
								disabled={working}
								onchange={(e) => (selectedKey = e.currentTarget.value)}
							>
								{#each writable as c (calendarKey(c))}
									<option value={calendarKey(c)}>{c.displayName ?? c.href}</option>
								{/each}
							</select>
						{/if}
						<button class="btn primary" disabled={working || !canImport} onclick={addToCalendar}>
							Add to calendar
						</button>
					{/if}
					{#if invitation.autoApply === 'manual'}
						<button class="btn" disabled={working} onclick={updateCalendar}>
							{cancelled(event) ? 'Mark cancelled in calendar' : 'Update calendar'}
						</button>
					{/if}
					{#if invitation.autoApply === 'applied'}
						<span class="state"><Icon name="check" size={16} />Applied</span>
					{:else if invitation.autoApply === 'pending'}
						<span class="state">Will be applied</span>
					{/if}
				</div>
				<div class="feedback" role="status">
					{#if op && op.status !== 'failed'}
						<span>{(!reverting && PENDING_TEXT[op.intent.kind]) || 'Saving…'}</span>
					{:else if note}
						<span class="problem">{note.text}. See Unsynced changes.</span>
					{:else if op?.error}
						<span class="problem">{op.error}</span>
					{:else if replyOnly}
						<span>Reply sent; not saved to a calendar</span>
					{:else if invitation.stored?.replyState === 'cancelled' && !working}
						{@const partstat = invitation.stored.partstat}
						{@const prior = invitation.stored.revertPartstat}
						<span class="problem">
							Reply not sent
							{#if invitation.method === 'request' && resendable(partstat)}
								<button class="link" onclick={() => reply(partstat)}>Send reply again</button>
							{/if}
							{#if invitation.method === 'request' && revertible(prior) && prior !== partstat}
								<button class="link" onclick={() => revert(prior)}>Revert</button>
							{/if}
						</span>
					{:else if invitation.stored && invitation.method !== 'request'}
						<span>In your calendar</span>
					{:else if answer && ANSWERS[answer]}
						<span>{ANSWERS[answer]}</span>
					{/if}
					{#if actionError}<span class="problem">{actionError}</span>{/if}
					{#if invitation.target === null}
						<span>
							No calendar set up.
							<a href="/settings/accounts/{accountId}#caldav-title">Set up CalDAV</a>
						</span>
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

<dialog
	bind:this={takeoverDialog}
	aria-labelledby={takeoverTitle}
	onclose={() => (takeover = null)}
>
	<h2 id={takeoverTitle}>Continue with a different organizer?</h2>
	<p>
		This update names {invitation?.events[0]?.organizer?.email ?? 'another organizer'} as the organizer,
		not the one your calendar has. Only continue if you trust this sender.
	</p>
	<div class="dialog-actions">
		<button class="btn" type="button" onclick={() => (takeover = null)}>Cancel</button>
		<button class="btn primary" type="button" onclick={proceed}>{takeover?.label}</button>
	</div>
</dialog>

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

	.btn.active {
		border-color: var(--accent);
		background: var(--accent-soft);
		color: var(--accent);
	}

	.btn.primary.active {
		background: var(--accent);
		color: var(--accent-ink);
	}

	.picker {
		max-width: 100%;
		height: 36px;
		padding: 0 8px;
		border: 1px solid var(--line-strong);
		border-radius: var(--radius);
		background: var(--surface);
		color: var(--ink);
	}

	.state {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		margin-left: 4px;
		color: var(--muted);
		font-size: 13px;
	}

	.feedback {
		display: grid;
		gap: 2px;
		padding: 0 14px 10px;
		color: var(--muted);
		font-size: 12.5px;
	}

	.feedback:not(:has(span)) {
		display: none;
	}

	.problem {
		color: var(--danger);
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

	dialog {
		width: min(460px, calc(100vw - 32px));
		padding: 20px 24px;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	dialog::backdrop {
		background: rgb(8 10 14 / 0.45);
	}

	@media (prefers-color-scheme: dark) {
		dialog::backdrop {
			background: rgb(0 0 0 / 0.7);
		}
	}

	dialog h2 {
		margin: 0 0 8px;
		font-size: 18px;
		font-weight: 650;
	}

	dialog p {
		overflow-wrap: anywhere;
	}

	.dialog-actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
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
