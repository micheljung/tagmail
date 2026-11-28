<script lang="ts">
	import type { CalendarDto, EventDetailDto } from '@tagmail/api-types';
	import { calendars, ops } from '../stores';
	import { api } from '../transport';
	import { errorText } from '../ui/errors';
	import Icon from '../ui/Icon.svelte';
	import {
		REMINDERS,
		addresses,
		draftOf,
		fieldsOf,
		newDraft,
		problem,
		reminderLabel,
		withStart,
		zones,
		type Draft,
		type EditorTarget
	} from './editor';
	import { untrack } from 'svelte';

	let { target, onclose }: { target: EditorTarget; onclose: (target: EditorTarget) => void } =
		$props();

	let dialog = $state<HTMLDialogElement>();
	let title = $state<HTMLInputElement>();
	let detail = $state<EventDetailDto | null>(null);
	let draft = $state<Draft | null>(null);
	let calendarKey = $state('');
	let guest = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let confirmingDelete = $state(false);
	const titleId = $props.id();

	const key = (c: Pick<CalendarDto, 'accountId' | 'href'>) => `${c.accountId}:${c.href}`;
	const writable = $derived(
		calendars.calendars.filter((c) => c.writable && (!detail || c.accountId === detail.accountId))
	);
	const chosen = $derived(writable.find((c) => key(c) === calendarKey));
	const organizer = $derived(detail?.organizer?.email ?? null);
	const invited = $derived(detail !== null && !detail.organizedByMe);
	const fixed = $derived(detail?.fixedTimes ?? false);
	const zoneList = $derived(zones(draft?.tzid ?? null));

	$effect(() => {
		dialog?.showModal();
	});

	$effect(() => {
		title?.focus();
	});

	$effect(() => {
		const wanted = target;
		const ctl = new AbortController();
		untrack(() => load(wanted, ctl.signal));
		return () => ctl.abort();
	});

	function load(target: EditorTarget, signal: AbortSignal): void {
		if (target.kind === 'new') {
			draft = newDraft(target.start, target.allDay);
			const preferred =
				calendars.calendars.find((c) => c.writable && c.isDefault && c.visible) ??
				calendars.calendars.find((c) => c.writable && c.visible) ??
				calendars.calendars.find((c) => c.writable);
			calendarKey = preferred ? key(preferred) : '';
		} else {
			api
				.calendarEvent(target.accountId, target.href, signal)
				.then((found) => {
					detail = found;
					draft = draftOf(found.fields);
					calendarKey = key({ accountId: found.accountId, href: found.calendarHref });
				})
				.catch((e: unknown) => {
					if (!signal.aborted) error = errorText(e);
				});
		}
	}

	function close(): void {
		dialog?.close();
	}

	function addGuest(): void {
		if (!draft) return;
		for (const a of addresses(guest)) {
			if (!draft.attendees.some((x) => x.toLowerCase() === a.toLowerCase())) {
				draft.attendees.push(a);
			}
		}
		guest = '';
	}

	function addReminder(value: string): void {
		const minutes = Number(value);
		if (draft && value !== '' && !draft.alarms.includes(minutes)) {
			draft.alarms = [...draft.alarms, minutes].sort((a, b) => a - b);
		}
	}

	async function run(work: () => Promise<void>): Promise<void> {
		busy = true;
		error = null;
		try {
			await work();
			close();
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = false;
		}
	}

	function save(e: SubmitEvent): void {
		e.preventDefault();
		if (!draft) return;
		if (guest.trim()) addGuest();
		const why = problem(draft) ?? (chosen ? null : 'Choose a calendar.');
		if (why || !chosen) {
			error = why;
			return;
		}
		const wanted = draft;
		const calendar = chosen;
		void run(async () => {
			if (!detail) {
				ops.track(
					await api.editEvent({
						opId: ops.newOpId(),
						kind: 'put',
						accountId: calendar.accountId,
						calendarHref: calendar.href,
						event: fieldsOf(wanted)
					})
				);
				return;
			}
			const fields = fieldsOf(wanted, detail.fields);
			const changed =
				JSON.stringify(fields) !== JSON.stringify(fieldsOf(draftOf(detail.fields), detail.fields));
			const base = {
				accountId: detail.accountId,
				calendarHref: detail.calendarHref,
				href: detail.href
			};
			const etag = detail.etag ?? undefined;
			if (changed) {
				ops.track(
					await api.editEvent({ ...base, opId: ops.newOpId(), kind: 'put', etag, event: fields })
				);
			}
			if (calendar.href !== detail.calendarHref) {
				ops.track(
					await api.editEvent({
						...base,
						opId: ops.newOpId(),
						kind: 'move',
						destCalendarHref: calendar.href,
						...(changed ? {} : { etag })
					})
				);
			}
		});
	}

	function remove(decline?: boolean): void {
		const event = detail;
		if (!event) return;
		void run(async () => {
			ops.track(
				await api.editEvent({
					opId: ops.newOpId(),
					kind: 'delete',
					accountId: event.accountId,
					calendarHref: event.calendarHref,
					href: event.href,
					etag: event.etag ?? undefined,
					...(decline === undefined ? {} : { sendDecline: decline })
				})
			);
		});
	}
</script>

<dialog
	bind:this={dialog}
	class="editor"
	aria-labelledby={titleId}
	onclose={() => onclose(target)}
	onclick={(e) => {
		if (e.target === dialog) close();
	}}
>
	<form onsubmit={save}>
		<header>
			<h2 id={titleId}>{target.kind === 'new' ? 'New event' : 'Edit event'}</h2>
			<button type="button" class="close" onclick={close} aria-label="Close">
				<Icon name="close" />
			</button>
		</header>
		{#if !draft}
			<p class="note" role="status">{error ?? 'Loading…'}</p>
		{:else}
			<div class="body">
				{#if detail?.recurring}
					<p class="note">This event repeats; changes apply to every occurrence.</p>
				{/if}
				{#if invited}
					<p class="note">
						{detail?.organizer?.name ?? organizer} organizes this event; your changes stay in your calendar.
					</p>
				{/if}
				<label class="field">
					<span>Title</span>
					<input bind:this={title} bind:value={draft.summary} placeholder="Untitled" />
				</label>
				{#if fixed}
					<p class="note">
						This repeating event has exceptions, so its time cannot be changed here yet.
					</p>
				{/if}
				<label class="check">
					<input type="checkbox" bind:checked={draft.allDay} disabled={fixed} />
					All day
				</label>
				<div class="when">
					<label class="field">
						<span>Starts</span>
						<input
							type="date"
							value={draft.startDate}
							disabled={fixed}
							required
							onchange={(e) =>
								draft && (draft = withStart(draft, e.currentTarget.value, draft.startTime))}
						/>
					</label>
					{#if !draft.allDay}
						<label class="field">
							<span>at</span>
							<input
								type="time"
								value={draft.startTime}
								disabled={fixed}
								required
								onchange={(e) =>
									draft && (draft = withStart(draft, draft.startDate, e.currentTarget.value))}
							/>
						</label>
					{/if}
					<label class="field">
						<span>Ends</span>
						<input type="date" bind:value={draft.endDate} disabled={fixed} required />
					</label>
					{#if !draft.allDay}
						<label class="field">
							<span>at</span>
							<input type="time" bind:value={draft.endTime} disabled={fixed} required />
						</label>
					{/if}
				</div>
				{#if !draft.allDay}
					<label class="field">
						<span>Time zone</span>
						<select bind:value={draft.tzid} disabled={fixed}>
							{#if detail?.fields.tzid === null && !detail.fields.allDay}
								<option value={null}>Floating (same clock time everywhere)</option>
							{/if}
							{#each zoneList as zone (zone)}
								<option value={zone}>{zone}</option>
							{/each}
						</select>
					</label>
				{/if}
				<label class="field">
					<span>Calendar</span>
					<select bind:value={calendarKey}>
						{#each writable as c (key(c))}
							<option value={key(c)}>{c.displayName ?? c.href}</option>
						{/each}
					</select>
				</label>
				<label class="field">
					<span>Location</span>
					<input bind:value={draft.location} />
				</label>
				<label class="field">
					<span>Description</span>
					<textarea bind:value={draft.description} rows="3"></textarea>
				</label>
				<fieldset>
					<legend>Guests</legend>
					{#if draft.attendees.length > 0}
						<ul class="list">
							{#each draft.attendees as a (a)}
								<li>
									<span>{a}</span>
									{#if !invited}
										<button
											type="button"
											class="remove"
											aria-label="Remove {a}"
											onclick={() =>
												draft && (draft.attendees = draft.attendees.filter((x) => x !== a))}
										>
											<Icon name="close" size={16} />
										</button>
									{/if}
								</li>
							{/each}
						</ul>
					{/if}
					{#if !invited}
						<div class="add">
							<input
								type="email"
								multiple
								bind:value={guest}
								aria-label="Add guest"
								placeholder="name@example.com"
								onkeydown={(e) => {
									if (e.key === 'Enter' && guest.trim()) {
										e.preventDefault();
										addGuest();
									}
								}}
							/>
							<button type="button" class="btn" onclick={addGuest} disabled={!guest.trim()}>
								Add
							</button>
						</div>
					{/if}
				</fieldset>
				<fieldset>
					<legend>Reminders</legend>
					{#if draft.alarms.length > 0}
						<ul class="list">
							{#each draft.alarms as minutes (minutes)}
								<li>
									<span>{reminderLabel(minutes)}</span>
									<button
										type="button"
										class="remove"
										aria-label="Remove reminder {reminderLabel(minutes)}"
										onclick={() =>
											draft && (draft.alarms = draft.alarms.filter((m) => m !== minutes))}
									>
										<Icon name="close" size={16} />
									</button>
								</li>
							{/each}
						</ul>
					{/if}
					<select
						aria-label="Add reminder"
						value=""
						onchange={(e) => {
							addReminder(e.currentTarget.value);
							e.currentTarget.value = '';
						}}
					>
						<option value="">Add reminder…</option>
						{#each REMINDERS.filter((r) => !draft?.alarms.includes(r.minutes)) as r (r.minutes)}
							<option value={String(r.minutes)}>{r.label}</option>
						{/each}
					</select>
				</fieldset>
				{#if error}<p class="error" role="alert">{error}</p>{/if}
			</div>
			<footer>
				{#if detail}
					{#if confirmingDelete && invited}
						<button type="button" class="btn danger" onclick={() => remove(true)} disabled={busy}>
							Delete and send decline
						</button>
						<button type="button" class="btn danger" onclick={() => remove(false)} disabled={busy}>
							Delete only
						</button>
						<button type="button" class="btn" onclick={() => (confirmingDelete = false)}>
							Keep
						</button>
					{:else if confirmingDelete}
						<button type="button" class="btn danger" onclick={() => remove()} disabled={busy}>
							Delete event
						</button>
						<button type="button" class="btn" onclick={() => (confirmingDelete = false)}>
							Keep
						</button>
					{:else}
						<button type="button" class="btn danger" onclick={() => (confirmingDelete = true)}>
							Delete
						</button>
					{/if}
				{/if}
				<span class="grow"></span>
				<button type="button" class="btn" onclick={close}>Cancel</button>
				<button type="submit" class="btn primary" disabled={busy}>Save</button>
			</footer>
		{/if}
	</form>
</dialog>

<style>
	dialog {
		width: min(560px, calc(100vw - 32px));
		max-height: calc(100dvh - 48px);
		padding: 0;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	dialog::backdrop {
		background: rgb(8 10 14 / 0.45);
	}

	form {
		display: flex;
		flex-direction: column;
		max-height: inherit;
	}

	header,
	footer {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 14px 18px 8px 24px;
	}

	footer {
		flex-wrap: wrap;
		padding: 12px 18px 16px 24px;
		border-top: 1px solid var(--line);
	}

	header {
		justify-content: space-between;
	}

	h2 {
		margin: 0;
		font-size: 18px;
		font-weight: 650;
	}

	.close,
	.remove {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: var(--ink-2);
		cursor: pointer;
	}

	.close:hover,
	.remove:hover {
		background: var(--hover);
	}

	.body {
		display: grid;
		gap: 12px;
		padding: 4px 24px 16px;
		overflow-y: auto;
	}

	.field {
		display: grid;
		gap: 4px;
		min-width: 0;
	}

	.field > span,
	legend {
		color: var(--muted);
		font-size: 12px;
		font-weight: 600;
	}

	input:not([type='checkbox']),
	select,
	textarea {
		width: 100%;
		min-height: 38px;
		padding: 6px 10px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface);
		color: var(--ink);
		font: inherit;
	}

	textarea {
		resize: vertical;
	}

	.check {
		display: flex;
		align-items: center;
		gap: 8px;
	}

	.when {
		display: grid;
		grid-template-columns: minmax(0, 1.3fr) minmax(0, 1fr);
		gap: 8px 10px;
	}

	fieldset {
		display: grid;
		gap: 6px;
		margin: 0;
		padding: 0;
		border: 0;
	}

	.list {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}

	.list li {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding-left: 10px;
		border-radius: 8px;
		background: var(--surface-2);
	}

	.list li span {
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.add {
		display: flex;
		gap: 8px;
	}

	.note {
		margin: 0;
		color: var(--muted);
		font-size: 13px;
	}

	header + .note {
		padding: 12px 24px 24px;
	}

	.error {
		margin: 0;
		padding: 8px 10px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
	}

	.grow {
		flex: 1;
	}

	@media (max-width: 720px) {
		dialog {
			width: 100vw;
			max-width: 100vw;
			height: 100dvh;
			max-height: 100dvh;
			margin: 0;
			border: 0;
			border-radius: 0;
		}

		form {
			height: 100%;
		}

		.body {
			flex: 1;
		}

		input:not([type='checkbox']),
		select {
			min-height: 44px;
		}

		.close,
		.remove {
			width: 44px;
			height: 44px;
		}
	}
</style>
