<script lang="ts">
	import { page } from '$app/state';
	import type { CalendarDto, EventInstanceDto } from '@tagmail/api-types';
	import {
		VIEWS,
		addDays,
		byDay,
		calendarColor,
		dayKey,
		instanceKey,
		parseDate,
		parseView,
		placeDay,
		shownCalendars,
		step,
		viewDays,
		wallMinutes,
		type CalendarView
	} from '#lib/calendar/views.ts';
	import { APP_NAME } from '#lib/document-title.ts';
	import { calendars } from '#lib/stores/index.ts';
	import { api } from '#lib/transport/index.ts';
	import { errorText } from '#lib/ui/errors.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import { onMount } from 'svelte';

	const HOUR_PX = 48;
	const FIRST_HOUR = 7;
	const MONTH_CHIPS = 3;
	const LABELS: Record<CalendarView, string> = {
		agenda: 'Agenda',
		day: 'Day',
		week: 'Week',
		month: 'Month'
	};

	const time = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' });
	const hourLabel = new Intl.DateTimeFormat(undefined, { hour: 'numeric' });
	const weekday = new Intl.DateTimeFormat(undefined, { weekday: 'short' });
	const longDay = new Intl.DateTimeFormat(undefined, {
		weekday: 'long',
		day: 'numeric',
		month: 'long'
	});
	const span = new Intl.DateTimeFormat(undefined, {
		day: 'numeric',
		month: 'long',
		year: 'numeric'
	});
	const dateTime = new Intl.DateTimeFormat(undefined, {
		month: 'short',
		day: 'numeric',
		hour: 'numeric',
		minute: '2-digit'
	});
	const monthTitle = new Intl.DateTimeFormat(undefined, { month: 'long', year: 'numeric' });

	let loaded = $state(false);
	let error = $state<string | null>(null);
	let events = $state<EventInstanceDto[]>([]);
	let now = $state(new Date());
	let scroller = $state<HTMLDivElement>();

	const view = $derived(parseView(page.url.searchParams.get('view')));
	const date = $derived(parseDate(page.url.searchParams.get('date'), now));
	const days = $derived(viewDays(view, date));
	const from = $derived(days[0].getTime() / 1000);
	const to = $derived(addDays(days[days.length - 1], 1).getTime() / 1000);
	const wanted = $derived(JSON.stringify(shownCalendars(calendars.calendars)));
	const perDay = $derived(byDay(events));
	const todayKey = $derived(dayKey(now));
	const title = $derived.by(() => {
		if (view === 'month') return monthTitle.format(date);
		if (view === 'day') return span.format(date);
		return span.formatRange(days[0], days[days.length - 1]);
	});
	const calendarOf = $derived(
		new Map(calendars.calendars.map((c) => [`${c.accountId}:${c.href}`, c]))
	);

	onMount(() => {
		calendars
			.load()
			.then(() => (loaded = true))
			.catch((e: unknown) => (error = errorText(e)));
		const tick = setInterval(() => (now = new Date()), 60_000);
		return () => clearInterval(tick);
	});

	$effect(() => {
		if (!loaded) return;
		const targets = JSON.parse(wanted) as ReturnType<typeof shownCalendars>;
		if (targets === null) {
			events = [];
			return;
		}
		const ctl = new AbortController();
		api
			.calendarEvents(from, to, targets, ctl.signal)
			.then((found) => {
				events = found;
				error = null;
			})
			.catch((e: unknown) => {
				if (!ctl.signal.aborted) error = errorText(e);
			});
		return () => ctl.abort();
	});

	$effect(() => {
		if (scroller && (view === 'day' || view === 'week'))
			scroller.scrollTop = FIRST_HOUR * HOUR_PX - 8;
	});

	function link(v: CalendarView, d: Date): string {
		return `/calendar?view=${v}&date=${dayKey(d)}`;
	}

	function calendarOfEvent(e: EventInstanceDto): CalendarDto | undefined {
		return calendarOf.get(`${e.accountId}:${e.calendarHref}`);
	}

	function colorOf(e: EventInstanceDto): string {
		const c = calendarOfEvent(e);
		return c ? calendarColor(c) : 'var(--accent)';
	}

	function when(e: EventInstanceDto): string {
		if (e.allDay) return 'All day';
		const [from, to] = [new Date(e.start * 1000), new Date(Math.max(e.end, e.start) * 1000)];
		const oneDay = dayKey(from) === dayKey(new Date(Math.max(to.getTime() - 1, from.getTime())));
		return (oneDay ? time : dateTime).formatRange(from, to);
	}

	function details(e: EventInstanceDto): string {
		const c = calendarOfEvent(e);
		return [when(e), e.location, c?.displayName ?? c?.href].filter(Boolean).join(', ');
	}

	const eventsOf = (d: Date) => perDay.get(dayKey(d)) ?? [];
	const agendaDays = $derived(days.filter((d) => eventsOf(d).length > 0));
	const nowMinutes = $derived(wallMinutes(now));
</script>

<svelte:head>
	<title>Calendar · {APP_NAME}</title>
</svelte:head>

{#snippet chip(e: EventInstanceDto)}
	<span
		class="chip"
		class:all-day={e.allDay}
		class:cancelled={e.status === 'cancelled'}
		class:declined={e.partstat === 'declined'}
		style:--c={colorOf(e)}
		data-event={e.uid}
		title="{e.summary ?? 'Untitled'}, {details(e)}"
	>
		{#if !e.allDay}<span class="at" aria-hidden="true">
				{time.format(new Date(e.start * 1000))}
			</span>{/if}
		<span class="label">{e.summary ?? 'Untitled'}</span>
		<span class="sr-only">, {details(e)}</span>
	</span>
{/snippet}

<div class="calendar" data-view={view}>
	<header class="toolbar">
		<div class="nav">
			<a class="go-today" href={link(view, now)}>Today</a>
			<a class="step" href={link(view, step(view, date, -1))} aria-label="Previous">
				<Icon name="left" size={20} />
			</a>
			<a class="step" href={link(view, step(view, date, 1))} aria-label="Next">
				<Icon name="right" size={20} />
			</a>
			<h1 tabindex="-1">{title}</h1>
		</div>
		<nav class="views" aria-label="Calendar view">
			{#each VIEWS as v (v)}
				<a href={link(v, date)} aria-current={v === view ? 'page' : undefined}>{LABELS[v]}</a>
			{/each}
		</nav>
	</header>

	{#if error}<p class="error" role="alert">{error}</p>{/if}

	{#if loaded && calendars.calendars.length === 0}
		<div class="empty">
			<Icon name="calendar" size={32} />
			<p>No calendars yet. Set up CalDAV for an account to see its events here.</p>
			<a href="/settings/accounts">Accounts</a>
		</div>
	{:else if view === 'agenda'}
		<ol class="agenda" aria-label="Agenda">
			{#each agendaDays as d (dayKey(d))}
				<li class:today={dayKey(d) === todayKey}>
					<h2><a href={link('day', d)}>{longDay.format(d)}</a></h2>
					<ul>
						{#each eventsOf(d) as e (instanceKey(e))}
							{@const c = calendarOfEvent(e)}
							<li
								class="item"
								class:cancelled={e.status === 'cancelled'}
								style:--c={colorOf(e)}
								data-event={e.uid}
							>
								<span class="when">{when(e)}</span>
								<span class="what">
									<strong>{e.summary ?? 'Untitled'}</strong>
									{#if e.location}<span>{e.location}</span>{/if}
								</span>
								<span class="cal">{c?.displayName ?? c?.href ?? ''}</span>
							</li>
						{/each}
					</ul>
				</li>
			{:else}
				<li class="nothing">Nothing in these {days.length} days</li>
			{/each}
		</ol>
	{:else if view === 'month'}
		<div class="month" role="table" aria-label={title}>
			<div class="weekdays" role="row">
				{#each days.slice(0, 7) as d (dayKey(d))}
					<span role="columnheader">{weekday.format(d)}</span>
				{/each}
			</div>
			{#each [0, 1, 2, 3, 4, 5] as row (row)}
				<div class="week" role="row">
					{#each days.slice(row * 7, row * 7 + 7) as d (dayKey(d))}
						{@const list = eventsOf(d)}
						<div
							class="cell"
							role="cell"
							class:other={d.getMonth() !== date.getMonth()}
							class:today={dayKey(d) === todayKey}
							data-day={dayKey(d)}
						>
							<a class="num" href={link('day', d)} aria-label={longDay.format(d)}>{d.getDate()}</a>
							<div class="chips">
								{#each list.slice(0, MONTH_CHIPS) as e (instanceKey(e))}
									{@render chip(e)}
								{/each}
							</div>
							{#if list.length > MONTH_CHIPS}
								<a class="more" href={link('day', d)}>+{list.length - MONTH_CHIPS} more</a>
							{/if}
						</div>
					{/each}
				</div>
			{/each}
		</div>
	{:else}
		<div class="grid" style:--days={days.length} style:--hour="{HOUR_PX}px">
			<div class="heads">
				<span class="gutter"></span>
				{#each days as d (dayKey(d))}
					<a class="head" class:today={dayKey(d) === todayKey} href={link('day', d)}>
						<span>{weekday.format(d)}</span>
						<strong>{d.getDate()}</strong>
					</a>
				{/each}
			</div>
			<div class="all-days" role="group" aria-label="All day">
				<span class="gutter">All day</span>
				{#each days as d (dayKey(d))}
					<div class="slot" data-day={dayKey(d)}>
						{#each eventsOf(d).filter((e) => e.allDay) as e (instanceKey(e))}
							{@render chip(e)}
						{/each}
					</div>
				{/each}
			</div>
			<div class="scroll" bind:this={scroller}>
				<div class="hours" aria-hidden="true">
					{#each Array.from({ length: 24 }, (_, h) => h) as h (h)}
						<span style:--h={h}>{h === 0 ? '' : hourLabel.format(new Date(2000, 0, 1, h))}</span>
					{/each}
				</div>
				{#each days as d (dayKey(d))}
					<div class="column" data-day={dayKey(d)}>
						{#each placeDay(eventsOf(d), d) as p (instanceKey(p.event))}
							<div
								class="placed"
								style:--top={p.top / 60}
								style:--len={p.height / 60}
								style:--lane={p.lane}
								style:--lanes={p.lanes}
							>
								{@render chip(p.event)}
							</div>
						{/each}
						{#if dayKey(d) === todayKey}
							<span class="now" style:--top={nowMinutes / 60} aria-hidden="true"></span>
						{/if}
					</div>
				{/each}
			</div>
		</div>
	{/if}
</div>

<style>
	.calendar {
		display: flex;
		flex-direction: column;
		min-height: 0;
	}

	.toolbar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 10px 16px;
		padding: 14px 20px;
		border-bottom: 1px solid var(--line);
	}

	.nav {
		display: flex;
		align-items: center;
		gap: 4px;
		min-width: 0;
	}

	.go-today {
		height: 36px;
		margin-right: 8px;
		padding: 0 16px;
		border: 1px solid var(--line-strong);
		border-radius: 999px;
		color: var(--ink);
		font-weight: 600;
		line-height: 34px;
		text-decoration: none;
	}

	.step {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border-radius: 50%;
		color: var(--ink-2);
	}

	.go-today:hover,
	.step:hover {
		background: var(--hover);
	}

	h1 {
		margin: 0 0 0 8px;
		overflow: hidden;
		font-size: 20px;
		font-weight: 600;
		letter-spacing: -0.01em;
		white-space: nowrap;
		text-overflow: ellipsis;
		outline: none;
	}

	.views {
		display: flex;
		gap: 2px;
		padding: 3px;
		border-radius: 12px;
		background: var(--hover);
	}

	.views a {
		padding: 6px 14px;
		border-radius: 9px;
		color: var(--ink-2);
		font-weight: 560;
		text-decoration: none;
	}

	.views a[aria-current='page'] {
		background: var(--surface);
		color: var(--ink);
		box-shadow: var(--shadow);
		font-weight: 650;
	}

	.error {
		margin: 12px 20px 0;
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
	}

	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 8px;
		padding: 48px 24px;
		color: var(--muted);
		text-align: center;
	}

	.empty p {
		margin: 0;
	}

	.chip {
		display: flex;
		align-items: baseline;
		gap: 6px;
		min-width: 0;
		padding: 2px 8px;
		border-left: 3px solid var(--c);
		border-radius: 6px;
		background: color-mix(in oklab, var(--c) 16%, var(--surface));
		color: var(--ink);
		font-size: 12.5px;
		line-height: 1.35;
	}

	.chip.all-day {
		border-left: 0;
		background: var(--c);
		color: #fff;
		font-weight: 600;
	}

	.chip .at {
		flex: none;
		color: var(--ink-2);
		font-variant-numeric: tabular-nums;
	}

	.chip .label {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.cancelled .label,
	.cancelled strong {
		text-decoration: line-through;
	}

	.declined {
		opacity: 0.6;
	}

	/* Agenda */
	.agenda {
		margin: 0;
		padding: 8px 20px 24px;
		overflow-y: auto;
		list-style: none;
	}

	.agenda > li {
		display: grid;
		grid-template-columns: 200px minmax(0, 1fr);
		gap: 12px;
		padding: 12px 0;
		border-bottom: 1px solid var(--line);
	}

	.agenda h2 {
		margin: 6px 0 0;
		font-size: 14px;
		font-weight: 600;
	}

	.agenda h2 a {
		color: var(--ink);
		text-decoration: none;
	}

	.agenda .today h2 a {
		color: var(--accent);
	}

	.agenda ul {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}

	.item {
		display: grid;
		grid-template-columns: 150px minmax(0, 1fr) auto;
		align-items: center;
		gap: 12px;
		padding: 6px 10px;
		border-left: 4px solid var(--c);
		border-radius: 8px;
	}

	.item:hover {
		background: var(--hover);
	}

	.when {
		color: var(--ink-2);
		font-variant-numeric: tabular-nums;
	}

	.what {
		display: grid;
		min-width: 0;
	}

	.what strong {
		overflow: hidden;
		font-weight: 600;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.what span,
	.cal {
		color: var(--muted);
		font-size: 13px;
	}

	.nothing {
		padding: 32px 0;
		color: var(--muted);
		text-align: center;
	}

	/* Month */
	.month {
		display: grid;
		flex: 1;
		grid-template-rows: auto repeat(6, minmax(0, 1fr));
		min-height: 0;
	}

	.weekdays,
	.week {
		display: grid;
		grid-template-columns: repeat(7, minmax(0, 1fr));
	}

	.weekdays span {
		padding: 8px 10px 4px;
		color: var(--muted);
		font-size: 11px;
		font-weight: 650;
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}

	.cell {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-height: 0;
		padding: 4px 6px 6px;
		overflow: hidden;
		border-top: 1px solid var(--line);
		border-left: 1px solid var(--line);
	}

	.cell:first-child {
		border-left: 0;
	}

	.cell.other {
		background: var(--surface-2);
	}

	.cell.other .num {
		color: var(--faint);
	}

	.num {
		display: grid;
		place-items: center;
		align-self: flex-start;
		width: 26px;
		height: 26px;
		border-radius: 50%;
		color: var(--ink-2);
		font-size: 12.5px;
		font-weight: 600;
		text-decoration: none;
	}

	.num:hover {
		background: var(--hover);
	}

	.cell.today .num {
		background: var(--accent);
		color: var(--accent-ink);
	}

	.chips {
		display: grid;
		gap: 2px;
		min-width: 0;
	}

	.more {
		padding-left: 8px;
		color: var(--muted);
		font-size: 12px;
		text-decoration: none;
	}

	/* Day and week */
	.grid {
		--gutter: 56px;
		display: flex;
		flex: 1;
		flex-direction: column;
		min-height: 0;
	}

	.heads,
	.all-days {
		display: grid;
		grid-template-columns: var(--gutter) repeat(var(--days), minmax(0, 1fr));
		padding-right: 8px;
	}

	.head {
		display: flex;
		flex-direction: column;
		align-items: center;
		padding: 8px 0 6px;
		color: var(--muted);
		font-size: 11px;
		font-weight: 650;
		letter-spacing: 0.06em;
		text-decoration: none;
		text-transform: uppercase;
	}

	.head strong {
		display: grid;
		place-items: center;
		width: 34px;
		height: 34px;
		border-radius: 50%;
		color: var(--ink);
		font-size: 19px;
		font-weight: 560;
		letter-spacing: 0;
	}

	.head.today strong {
		background: var(--accent);
		color: var(--accent-ink);
	}

	.all-days {
		border-bottom: 1px solid var(--line-strong);
	}

	.gutter {
		color: var(--muted);
		font-size: 11px;
		text-align: right;
		padding: 4px 8px 4px 0;
	}

	.slot {
		display: grid;
		align-content: start;
		gap: 2px;
		min-width: 0;
		min-height: 28px;
		padding: 2px;
		border-left: 1px solid var(--line);
	}

	.scroll {
		position: relative;
		display: grid;
		flex: 1;
		grid-template-columns: var(--gutter) repeat(var(--days), minmax(0, 1fr));
		min-height: 0;
		padding-right: 8px;
		overflow-y: auto;
	}

	.hours {
		position: relative;
		height: calc(24 * var(--hour));
	}

	.hours span {
		position: absolute;
		top: calc(var(--h) * var(--hour) - 0.7em);
		right: 8px;
		color: var(--muted);
		font-size: 11px;
	}

	.column {
		position: relative;
		height: calc(24 * var(--hour));
		border-left: 1px solid var(--line);
		background: repeating-linear-gradient(
			to bottom,
			var(--line) 0 1px,
			transparent 1px var(--hour)
		);
	}

	.placed {
		position: absolute;
		top: calc(var(--top) * var(--hour));
		left: calc(var(--lane) / var(--lanes) * 100%);
		width: calc(100% / var(--lanes) - 2px);
		height: max(calc(var(--len) * var(--hour) - 1px), 20px);
		padding: 0 0 1px;
	}

	.placed .chip {
		flex-wrap: wrap;
		align-content: flex-start;
		height: 100%;
		overflow: hidden;
	}

	.placed .label {
		flex: 1 1 100%;
		order: -1;
		font-weight: 600;
	}

	.now {
		position: absolute;
		top: calc(var(--top) * var(--hour));
		right: 0;
		left: 0;
		height: 2px;
		background: var(--danger);
	}

	@media (max-width: 720px) {
		.toolbar {
			padding: 10px 12px;
		}

		.nav {
			flex: 1 1 100%;
		}

		h1 {
			font-size: 16px;
			line-height: 1.25;
			white-space: normal;
		}

		.views {
			flex: 1 1 100%;
		}

		.views a {
			display: grid;
			flex: 1;
			place-items: center;
			min-height: 44px;
			padding: 0;
		}

		.go-today,
		.step {
			height: 44px;
			line-height: 42px;
		}

		.step {
			width: 44px;
		}

		.agenda {
			padding: 4px 12px 16px;
		}

		.agenda > li,
		.item {
			grid-template-columns: minmax(0, 1fr);
			gap: 2px;
		}

		.cal {
			display: none;
		}

		.cell {
			align-items: center;
			padding: 2px 0 4px;
		}

		.num {
			align-self: center;
		}

		.chips {
			display: flex;
			flex-wrap: wrap;
			justify-content: center;
			gap: 3px;
		}

		.cell .chip {
			width: 7px;
			height: 7px;
			padding: 0;
			border: 0;
			border-radius: 50%;
			background: var(--c);
		}

		.cell .chip > span:not(.sr-only),
		.more {
			display: none;
		}

		.grid {
			--gutter: 40px;
		}

		.placed .chip {
			padding: 1px 3px;
			font-size: 11px;
		}

		.placed .chip .at {
			display: none;
		}
	}
</style>
