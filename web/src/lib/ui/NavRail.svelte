<script lang="ts">
	import { page } from '$app/state';
	import { SvelteSet } from 'svelte/reactivity';
	import { calendarColor, swatchValue } from '../calendar/views';
	import { labelColor } from '../format';
	import { accounts, calendars, isInbox, mailboxLabel, nav, theme, toasts } from '../stores';
	import type { CalendarDto } from '@tagmail/api-types';
	import { isDesktop, type MailboxDto } from '../transport';
	import { accountParam, withAccount } from '../views';
	import AccountMarker from './AccountMarker.svelte';
	import { errorText } from './errors';
	import Icon, { type IconName } from './Icon.svelte';

	const inApp = isDesktop();

	let {
		open = false,
		onnavigate,
		onhelp,
		onlogout,
		oncompose
	}: {
		open?: boolean;
		onnavigate: () => void;
		oncompose: () => void;
		onhelp: () => void;
		onlogout: () => void;
	} = $props();

	const STARRED = 'is:starred';
	const SENT = 'in:sent';

	/** The account the open list is scoped to; unified entries are current only without one. */
	const scope = $derived(accountParam(page.url));

	const items: { href: string; label: () => string; icon: IconName; match: () => boolean }[] = [
		{
			href: '/inbox',
			label: () => (accounts.multi ? 'All inboxes' : 'Inbox'),
			icon: 'inbox',
			match: () => page.url.pathname === '/inbox' && scope === null
		},
		{
			href: `/search?q=${encodeURIComponent(STARRED)}`,
			label: () => 'Starred',
			icon: 'star',
			match: () =>
				page.url.pathname === '/search' &&
				page.url.searchParams.get('q') === STARRED &&
				scope === null
		},
		{
			href: `/search?q=${encodeURIComponent(SENT)}`,
			label: () => 'Sent',
			icon: 'send',
			match: () =>
				page.url.pathname === '/search' && page.url.searchParams.get('q') === SENT && scope === null
		}
	];

	/** Collapsed account groups. */
	const collapsed = new SvelteSet<number>();

	function folderIcon(m: MailboxDto): IconName {
		if (isInbox(m)) return 'inbox';
		return m.specialUse === null ? 'folder' : specialIcon(m);
	}

	/** An account's Inbox, its own folders, then its special-use ones. */
	function mailboxesOf(account: number): MailboxDto[] {
		const own = (m: MailboxDto) => m.accountId === account;
		return [
			...nav.mailboxes.filter((m) => own(m) && isInbox(m)),
			...nav.folders.filter(own),
			...nav.specials.filter(own)
		];
	}

	let moreOpen = $state(false);

	const currentMailbox = $derived(
		page.url.pathname.startsWith('/mailbox/') ? Number(page.params.id) : null
	);

	function specialIcon(m: MailboxDto): IconName {
		switch (m.specialUse) {
			case 'trash':
				return 'trash';
			case 'junk':
				return 'alert';
			case 'drafts':
				return 'pencil';
			default:
				return 'archive';
		}
	}

	const onCalendar = $derived(page.url.pathname.startsWith('/calendar'));

	function patchCalendar(c: CalendarDto, change: { color?: string; visible?: boolean }) {
		calendars
			.patch({ accountId: c.accountId, href: c.href, ...change })
			.catch((e: unknown) => toasts.show(errorText(e)));
	}

	const currentLabel = $derived(
		page.url.pathname.startsWith('/label/') ? decodeURIComponent(page.params.name ?? '') : null
	);
</script>

<nav class="rail" class:open aria-label={onCalendar ? 'Calendars' : 'Mailboxes'}>
	<div class="switch" role="group" aria-label="Mail or calendar">
		<a href="/inbox" aria-current={onCalendar ? undefined : 'true'} onclick={onnavigate}>
			<Icon name="inbox" size={18} />
			<span>Mail</span>
		</a>
		<a href="/calendar" aria-current={onCalendar ? 'true' : undefined} onclick={onnavigate}>
			<Icon name="calendar" size={18} />
			<span>Calendar</span>
		</a>
	</div>
	{#if onCalendar}
		<h2 class="heading">Calendars</h2>
		{#each accounts.accounts.filter((a) => calendars.of(a.id).length > 0) as account (account.id)}
			{#if accounts.multi}<h3 class="account">{account.name}</h3>{/if}
			<ul class="calendars" aria-label={accounts.multi ? account.name : 'Calendars'}>
				{#each calendars.of(account.id) as c (c.href)}
					{@const name = c.displayName ?? c.href}
					<li style:--c={calendarColor(c)}>
						<label>
							<input
								type="checkbox"
								checked={c.visible}
								onchange={(e) => patchCalendar(c, { visible: e.currentTarget.checked })}
							/>
							<span>{name}</span>
						</label>
						<input
							type="color"
							value={swatchValue(c)}
							aria-label="Colour of {name}"
							onchange={(e) => patchCalendar(c, { color: e.currentTarget.value })}
						/>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="none">Calendars of your accounts show up here</p>
		{/each}
	{:else}
		<button class="compose" onclick={oncompose} title="Compose (c)">
			<Icon name="pencil" />
			<span>Compose</span>
		</button>
		<ul class="primary">
			{#each items as item (item.href)}
				<li>
					<a href={item.href} aria-current={item.match() ? 'page' : undefined} onclick={onnavigate}>
						<Icon name={item.icon} />
						<span>{item.label()}</span>
					</a>
				</li>
			{/each}
			{#each accounts.multi ? [] : nav.folders as box (box.id)}
				<li>
					<a
						href="/mailbox/{box.id}"
						aria-current={currentMailbox === box.id ? 'page' : undefined}
						onclick={onnavigate}
					>
						<Icon name="folder" />
						<span>{box.name}</span>
					</a>
				</li>
			{/each}
			{#if !accounts.multi && nav.specials.length > 0}
				<li>
					<button
						class="more"
						aria-expanded={moreOpen || nav.specials.some((m) => m.id === currentMailbox)}
						aria-controls="nav-more"
						onclick={() => (moreOpen = !moreOpen)}
					>
						<Icon
							name={moreOpen || nav.specials.some((m) => m.id === currentMailbox) ? 'up' : 'down'}
						/>
						<span>More</span>
					</button>
				</li>
			{/if}
		</ul>
		{#if !accounts.multi && (moreOpen || nav.specials.some((m) => m.id === currentMailbox))}
			<ul class="primary" id="nav-more">
				{#each nav.specials as box (box.id)}
					<li>
						<a
							href="/mailbox/{box.id}"
							aria-current={currentMailbox === box.id ? 'page' : undefined}
							onclick={onnavigate}
						>
							<Icon name={specialIcon(box)} />
							<span>{mailboxLabel(box)}</span>
						</a>
					</li>
				{/each}
			</ul>
		{/if}

		<h2 class="heading">Labels</h2>
		{#if nav.labels.length > 0}
			<ul class="labels">
				{#each nav.labels as label (label.key)}
					<li>
						<a
							href="/label/{encodeURIComponent(label.key)}"
							aria-current={currentLabel === label.key && scope === null ? 'page' : undefined}
							onclick={onnavigate}
						>
							<span class="tag" style:--c={labelColor(label.key, label.color)} aria-hidden="true"
							></span>
							<span>{label.name}</span>
						</a>
					</li>
				{/each}
			</ul>
		{:else if nav.loaded}
			<p class="none">{nav.error ? "Labels couldn't load" : 'Labels you add show up here'}</p>
		{/if}

		{#if accounts.multi}
			<h2 class="heading">Accounts</h2>
			{#each accounts.accounts as account (account.id)}
				{@const open = !collapsed.has(account.id)}
				<button
					class="more group"
					aria-expanded={open}
					aria-controls="nav-account-{account.id}"
					onclick={() => (open ? collapsed.add(account.id) : collapsed.delete(account.id))}
				>
					<AccountMarker id={account.id} name={account.name} />
					<span>{account.name}</span>
					<Icon name={open ? 'up' : 'down'} size={16} />
				</button>
				{#if open}
					<ul class="primary nested" id="nav-account-{account.id}" aria-label={account.name}>
						{#each mailboxesOf(account.id) as box (box.id)}
							{@const href = isInbox(box)
								? withAccount('/inbox', account.id)
								: `/mailbox/${box.id}`}
							{@const current = isInbox(box)
								? page.url.pathname === '/inbox' && scope === account.id
								: currentMailbox === box.id}
							<li>
								<a {href} aria-current={current ? 'page' : undefined} onclick={onnavigate}>
									<Icon name={folderIcon(box)} />
									<span>{isInbox(box) || box.specialUse ? mailboxLabel(box) : box.name}</span>
								</a>
							</li>
						{/each}
						{#each nav.labels.filter((l) => l.accountIds.includes(account.id)) as label (label.key)}
							<li>
								<a
									href={withAccount(`/label/${encodeURIComponent(label.key)}`, account.id)}
									aria-current={currentLabel === label.key && scope === account.id
										? 'page'
										: undefined}
									onclick={onnavigate}
								>
									<span
										class="tag"
										style:--c={labelColor(label.key, label.color)}
										aria-hidden="true"
									></span>
									<span>{label.name}</span>
								</a>
							</li>
						{/each}
					</ul>
				{/if}
			{/each}
		{/if}
	{/if}

	<div class="grow"></div>
	<ul class="footer">
		<li>
			<a
				href="/settings/accounts"
				aria-current={page.url.pathname.startsWith('/settings/accounts') ? 'page' : undefined}
				onclick={onnavigate}
			>
				<Icon name="account" />
				<span>Accounts</span>
			</a>
		</li>
		<li>
			<a
				href="/settings/labels"
				aria-current={page.url.pathname === '/settings/labels' ? 'page' : undefined}
				onclick={onnavigate}
			>
				<Icon name="tag" />
				<span>Manage labels</span>
			</a>
		</li>
		<li>
			{#if inApp}
				<a
					href="/settings/desktop"
					aria-current={page.url.pathname === '/settings/desktop' ? 'page' : undefined}
					onclick={onnavigate}
				>
					<Icon name="devices" />
					<span>This device</span>
				</a>
			{:else}
				<a
					href="/settings/devices"
					aria-current={page.url.pathname === '/settings/devices' ? 'page' : undefined}
					onclick={onnavigate}
				>
					<Icon name="devices" />
					<span>Devices &amp; sessions</span>
				</a>
			{/if}
		</li>
		<li class="tools">
			<button onclick={() => theme.cycle()} aria-label="Switch light or dark theme" title="Theme">
				<Icon name={theme.choice === 'dark' ? 'sun' : 'moon'} size={18} />
			</button>
			<button onclick={onhelp} aria-label="Keyboard shortcuts" title="Keyboard shortcuts (?)">
				<Icon name="keyboard" size={18} />
			</button>
			{#if !inApp}
				<a
					href="/settings/notifications"
					aria-label="Notifications"
					title="Notifications"
					aria-current={page.url.pathname === '/settings/notifications' ? 'page' : undefined}
					onclick={onnavigate}
				>
					<Icon name="bell" size={18} />
				</a>
				<button onclick={onlogout} aria-label="Sign out" title="Sign out">
					<Icon name="logout" size={18} />
				</button>
			{/if}
		</li>
	</ul>
</nav>

<style>
	.rail {
		display: flex;
		flex-direction: column;
		height: 100%;
		padding: 4px 12px 12px 8px;
		overflow-y: auto;
	}

	.rail > * {
		flex-shrink: 0;
	}

	.compose {
		display: inline-flex;
		align-items: center;
		gap: 12px;
		align-self: flex-start;
		height: 52px;
		margin: 0 0 14px 4px;
		padding: 0 22px 0 18px;
		border: 0;
		border-radius: 16px;
		background: var(--accent-soft);
		color: var(--accent);
		font-weight: 650;
		font-size: 14.5px;
		cursor: pointer;
		transition:
			box-shadow 140ms var(--ease),
			transform 140ms var(--ease);
	}

	.compose:hover {
		box-shadow: var(--shadow);
	}

	.compose:active {
		transform: translateY(1px);
	}

	ul {
		margin: 0;
		padding: 0;
		list-style: none;
	}

	a,
	.more {
		display: flex;
		align-items: center;
		gap: 14px;
		height: 34px;
		padding: 0 14px 0 16px;
		border-radius: 0 999px 999px 0;
		color: var(--ink-2);
		text-decoration: none;
		font-weight: 520;
	}

	a span,
	.more span {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.more {
		width: 100%;
		border: 0;
		background: none;
		cursor: pointer;
	}

	a:hover,
	.more:hover {
		background: var(--hover);
	}

	a[aria-current='page'] {
		background: var(--accent-soft);
		color: var(--accent);
		font-weight: 680;
	}

	.heading {
		margin: 18px 0 6px 16px;
		color: var(--muted);
		font-size: 11px;
		font-weight: 650;
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.grow {
		flex: 1;
	}

	.switch {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 2px;
		margin: 0 0 14px 4px;
		padding: 3px;
		border-radius: 12px;
		background: color-mix(in oklab, var(--ink) 7%, transparent);
	}

	.switch a {
		justify-content: center;
		gap: 8px;
		height: 32px;
		padding: 0 10px;
		border-radius: 9px;
		font-size: 13.5px;
	}

	.switch a[aria-current='true'] {
		background: var(--surface);
		color: var(--ink);
		box-shadow: var(--shadow);
		font-weight: 650;
	}

	.account {
		margin: 10px 0 2px 16px;
		color: var(--ink-2);
		font-size: 13px;
		font-weight: 600;
	}

	.calendars li {
		display: flex;
		align-items: center;
		gap: 6px;
		padding-right: 6px;
		border-radius: 0 999px 999px 0;
	}

	.calendars li:hover {
		background: var(--hover);
	}

	.calendars label {
		display: flex;
		flex: 1;
		align-items: center;
		gap: 12px;
		min-width: 0;
		height: 34px;
		padding-left: 16px;
		color: var(--ink-2);
		font-weight: 520;
		cursor: pointer;
	}

	.calendars label span {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.calendars input[type='checkbox'] {
		width: 18px;
		height: 18px;
		margin: 0 2px 0 0;
		accent-color: var(--c);
	}

	.calendars input[type='color'] {
		flex: none;
		width: 22px;
		height: 22px;
		padding: 0;
		border: 1px solid var(--line-strong);
		border-radius: 50%;
		background: none;
		cursor: pointer;
	}

	.calendars input[type='color']::-webkit-color-swatch-wrapper {
		padding: 2px;
	}

	.calendars input[type='color']::-webkit-color-swatch {
		border: 0;
		border-radius: 50%;
	}

	.calendars input[type='color']::-moz-color-swatch {
		border: 0;
		border-radius: 50%;
	}

	.group {
		margin-top: 2px;
		font-weight: 600;
	}

	.group span {
		flex: 1;
		text-align: left;
	}

	.nested a {
		padding-left: 28px;
	}

	.tag {
		flex: none;
		width: 16px;
		height: 12px;
		margin: 0 2px;
		clip-path: polygon(5px 0, 100% 0, 100% 100%, 5px 100%, 0 50%);
		border-radius: 0 3px 3px 0;
		background: var(--c);
	}

	.none {
		margin: 4px 16px;
		color: var(--faint);
		font-size: 13px;
	}

	.footer {
		margin-top: 16px;
		padding-top: 10px;
		border-top: 1px solid var(--line);
	}

	.tools {
		display: flex;
		gap: 4px;
		padding: 8px 0 0 10px;
	}

	.tools :is(button, a) {
		display: grid;
		place-items: center;
		padding: 0;
		width: 36px;
		height: 36px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: var(--muted);
		cursor: pointer;
	}

	.tools :is(button, a):hover {
		background: var(--hover);
		color: var(--ink);
	}

	.tools a[aria-current='page'] {
		background: var(--accent-soft);
		color: var(--accent);
	}

	@media (pointer: coarse), (max-width: 720px) {
		a,
		.more,
		.calendars label {
			height: 44px;
		}

		.switch a {
			height: 40px;
		}

		.tools :is(button, a) {
			width: 44px;
			height: 44px;
		}
	}
</style>
