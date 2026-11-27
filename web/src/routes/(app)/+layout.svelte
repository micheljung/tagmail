<script lang="ts">
	import { afterNavigate, goto, invalidateAll } from '$app/navigation';
	import { page } from '$app/state';
	import { shortcuts } from '#lib/shortcuts/index.ts';
	import ComposeDock from '#lib/compose/ComposeDock.svelte';
	import { composer, outbox } from '#lib/compose/composer.svelte.ts';
	import {
		accounts,
		currentAccount,
		hub,
		lists,
		nav,
		ops,
		prefs,
		resetAppState
	} from '#lib/stores/index.ts';
	import { api } from '#lib/transport/index.ts';
	import DesktopBanners from '#lib/ui/DesktopBanners.svelte';
	import HelpOverlay from '#lib/ui/HelpOverlay.svelte';
	import Icon from '#lib/ui/Icon.svelte';
	import LabelJump from '#lib/ui/LabelJump.svelte';
	import LabelPicker from '#lib/ui/LabelPicker.svelte';
	import MovePicker from '#lib/ui/MovePicker.svelte';
	import Toaster from '#lib/ui/Toaster.svelte';
	import UnsyncedPanel from '#lib/ui/UnsyncedPanel.svelte';
	import Logo from '#lib/ui/Logo.svelte';
	import NavRail from '#lib/ui/NavRail.svelte';
	import QuickSettings from '#lib/ui/QuickSettings.svelte';
	import { navigate } from '#lib/ui/navigate.ts';
	import OfflineBanner from '#lib/pwa/OfflineBanner.svelte';
	import { connectivity } from '#lib/pwa/connectivity.svelte.ts';
	import { withAccount } from '#lib/views.ts';
	import { NAV_WIDTH_DEFAULT, NAV_WIDTH_MAX, NAV_WIDTH_MIN, clampNavWidth } from '#lib/prefs.ts';
	import SearchBar from '#lib/ui/SearchBar.svelte';
	import { selectionBar } from '#lib/ui/selectionBar.svelte.ts';
	import { onMount, tick } from 'svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();

	let search: SearchBar | undefined = $state();
	let helpOpen = $state(false);
	let labelJumpOpen = $state(false);
	let drawerOpen = $state(false);
	let compact = $state(false);
	let menuButton: HTMLButtonElement | undefined = $state();
	let navEl: HTMLDivElement | undefined = $state();
	/**
	 * `page.state.appIndex` of the current entry, so history.back() stays
	 * inside the app. Each entry keeps its own index: popstate and replacing
	 * navigations (which carry `page.state` over) restore it, and one to the
	 * same URL keeps it, rather than count.
	 */
	let appIndex = 0;
	let entered = false;

	const PHONE_QUERY = '(max-width: 720px)';
	const NAV_STEP = 8;
	/** The width while the handle is dragged; stored on release. */
	let dragWidth: number | null = $state(null);
	let dragFrom = { x: 0, width: 0 };
	const navWidth = $derived(dragWidth ?? prefs.navWidth);

	function resizeStart(e: PointerEvent) {
		if (e.button !== 0) return;
		e.preventDefault();
		(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
		dragFrom = { x: e.clientX, width: prefs.navWidth };
		dragWidth = prefs.navWidth;
	}

	function resizeMove(e: PointerEvent) {
		if (dragWidth === null) return;
		dragWidth = clampNavWidth(dragFrom.width + e.clientX - dragFrom.x);
	}

	function resizeEnd() {
		if (dragWidth === null) return;
		prefs.setNavWidth(dragWidth);
		dragWidth = null;
	}

	function resizeKey(e: KeyboardEvent) {
		const step = { ArrowLeft: -NAV_STEP, ArrowRight: NAV_STEP }[e.key];
		if (step === undefined) return;
		e.preventDefault();
		e.stopPropagation();
		prefs.setNavWidth(prefs.navWidth + step);
	}
	const onThread = $derived(page.url.pathname.startsWith('/thread/'));
	let unsyncedOpen = $state(false);

	/** A change still on its way after this long gets surfaced; quick ones never flash. */
	const STALE_MS = 3000;
	let now = $state(Date.now());

	const unsyncedFailed = $derived(ops.failedCount + outbox.failed.length);
	const unsynced = $derived(
		ops.unsynced.filter((o) => o.status === 'failed' || now - o.createdAt > STALE_MS).length +
			outbox.failed.length
	);

	$effect(() => {
		if (ops.unsynced.length === 0) return;
		const timer = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(timer);
	});

	const query = $derived(
		page.url.pathname === '/search' ? (page.url.searchParams.get('q') ?? '') : ''
	);

	afterNavigate((navigation) => {
		const pushed = entered && navigation.type !== 'popstate';
		entered = true;
		const kept = page.state.appIndex;
		if (kept !== undefined) {
			appIndex = kept;
			return;
		}
		const reloaded = navigation.from?.url.href === navigation.to?.url.href;
		if (!reloaded) appIndex = pushed ? appIndex + 1 : 0;
		void goto(page.url.href, { shallow: true, replace: true, state: { ...page.state, appIndex } });
	});

	function back() {
		if (appIndex > 0) history.back();
		else void navigate('/inbox');
	}

	function openDrawer() {
		drawerOpen = true;
		requestAnimationFrame(() => navEl?.querySelector<HTMLElement>('a, button')?.focus());
	}

	async function closeDrawer() {
		drawerOpen = false;
		await tick();
		menuButton?.focus();
	}

	/** Keeps Tab inside the open drawer; the rest of the page is inert behind it. */
	function trapTab(e: KeyboardEvent) {
		if (!navEl) return;
		const focusable = [...navEl.querySelectorAll<HTMLElement>('a[href], button:not(:disabled)')];
		if (focusable.length === 0) return;
		const first = focusable[0];
		const last = focusable[focusable.length - 1];
		const inside = navEl.contains(document.activeElement);
		if (e.shiftKey && (!inside || document.activeElement === first)) last.focus();
		else if (!e.shiftKey && (!inside || document.activeElement === last)) first.focus();
		else return;
		e.preventDefault();
	}

	onMount(() => {
		const mq = matchMedia(PHONE_QUERY);
		compact = mq.matches;
		prefs.phone = mq.matches;
		const onChange = () => {
			compact = mq.matches;
			prefs.phone = mq.matches;
			drawerOpen = false;
		};
		mq.addEventListener('change', onChange);
		api.onUnauthorized = (error) => {
			resetAppState();
			const next = encodeURIComponent(location.pathname + location.search);
			if (error.code === 'unpaired') void goto('/pair');
			else void goto(error.code === 'setup_required' ? '/setup' : `/login?next=${next}`);
		};
		void nav.load();
		void accounts.load();
		// Outbox and op events matter on every page, not only once a list loaded.
		if (!hub.running) hub.start(undefined);
		void ops.load().catch(() => undefined);
		void outbox.load().catch(() => undefined);
		void composer.loadIdentities().catch(() => undefined);
		const pop = shortcuts.push((action) => {
			switch (action) {
				case 'compose':
					composer.open();
					return true;
				case 'search':
					search?.focus();
					return true;
				case 'help':
					helpOpen = !helpOpen;
					return true;
				case 'goInbox':
					void goto(withAccount('/inbox', currentAccount(page.url)));
					return true;
				case 'goLabel':
					labelJumpOpen = true;
					return true;
				case 'back':
					if (page.url.pathname.startsWith('/thread/')) {
						void navigate(lists.currentPath);
						return true;
					}
					return false;
				case 'escape':
					if (
						document.activeElement instanceof HTMLElement &&
						document.activeElement !== document.body
					) {
						document.activeElement.blur();
						return true;
					}
					return false;
				default:
					return false;
			}
		});
		const stopRecovery = connectivity.onRecovered(() => {
			void invalidateAll();
			void nav.load();
			void accounts.load();
			for (const list of lists.all()) void list.load();
		});
		return () => {
			mq.removeEventListener('change', onChange);
			pop();
			stopRecovery();
		};
	});

	async function logout() {
		try {
			await api.logout();
		} finally {
			resetAppState();
			await goto('/login');
		}
	}
</script>

<svelte:window
	onkeydowncapture={(e) => {
		if (!drawerOpen) return;
		if (e.key === 'Tab') trapTab(e);
		if (e.key !== 'Escape') return;
		e.preventDefault();
		void closeDrawer();
	}}
/>

<a class="skip" href="#main">Skip to conversations</a>
<div
	class="shell"
	class:drawer={drawerOpen}
	class:selecting={selectionBar.active}
	style:--nav-w="{navWidth}px"
>
	<!-- On phones the list's selection bar takes the header's place. -->
	<header class="top" inert={compact && drawerOpen} hidden={selectionBar.active}>
		{#if compact && onThread}
			<button class="menu" aria-label="Back" onclick={back}>
				<Icon name="back" />
			</button>
		{:else}
			<button
				class="menu"
				aria-label="Menu"
				aria-expanded={drawerOpen}
				aria-controls="nav"
				bind:this={menuButton}
				onclick={() => (drawerOpen ? void closeDrawer() : openDrawer())}
			>
				<Icon name="menu" />
			</button>
		{/if}
		<div class="brand"><Logo /></div>
		<SearchBar bind:this={search} {query} />
		{#if unsynced > 0}
			<button
				class="unsynced"
				class:failed={unsyncedFailed > 0}
				onclick={() => (unsyncedOpen = true)}
				aria-haspopup="dialog"
			>
				<Icon name={unsyncedFailed > 0 ? 'alert' : 'sync'} size={16} />
				<span>{unsynced} unsynced</span>
			</button>
		{/if}
		<span
			class="live"
			class:on={hub.connected}
			role="status"
			title={hub.connected ? 'Live updates on' : 'Live updates paused'}
		>
			<span class="sr-only">{hub.connected ? 'Live updates on' : 'Live updates paused'}</span>
		</span>
		<QuickSettings />
	</header>
	<div
		class="side"
		id="nav"
		bind:this={navEl}
		inert={compact && !drawerOpen}
		role={compact ? 'dialog' : undefined}
		aria-modal={compact && drawerOpen ? 'true' : undefined}
		aria-label={compact ? 'Menu' : undefined}
	>
		<NavRail
			open={drawerOpen}
			onnavigate={() => (drawerOpen = false)}
			oncompose={() => {
				drawerOpen = false;
				composer.open();
			}}
			onhelp={() => (helpOpen = true)}
			onlogout={logout}
		/>
		<!-- A focusable separator is a widget (WAI-ARIA window splitter); Svelte lists it as static. -->
		<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
		<div
			class="resize"
			role="separator"
			aria-label="Resize navigation"
			aria-orientation="vertical"
			aria-controls="nav"
			aria-valuenow={navWidth}
			aria-valuemin={NAV_WIDTH_MIN}
			aria-valuemax={NAV_WIDTH_MAX}
			aria-valuetext="{navWidth} pixels"
			tabindex="0"
			title="Drag to resize, double-click to reset"
			onpointerdown={resizeStart}
			onpointermove={resizeMove}
			onpointerup={resizeEnd}
			onpointercancel={resizeEnd}
			onkeydown={resizeKey}
			ondblclick={() => prefs.setNavWidth(NAV_WIDTH_DEFAULT)}
		></div>
	</div>
	<button class="scrim" tabindex="-1" aria-label="Close menu" onclick={() => void closeDrawer()}
	></button>
	<main id="main" inert={compact && drawerOpen}>
		<DesktopBanners />
		<OfflineBanner />
		{#if accounts.notices.length > 0}
			<ul class="banners">
				{#each accounts.notices as notice (`${notice.accountId}:${notice.code}`)}
					<li role="status" data-notice={notice.code}>
						<Icon name="alert" size={16} />
						<span>
							{#if notice.code === 'labels_degraded'}
								<strong>{notice.name}</strong> drops labels it doesn't know, so some labels may not stick
								there.
							{:else}
								<strong>{notice.name}</strong> has no address to send from yet.
							{/if}
						</span>
						<a href="/settings/accounts/{notice.accountId}">Account settings</a>
					</li>
				{/each}
			</ul>
		{/if}
		{@render children()}
	</main>
</div>

<HelpOverlay bind:open={helpOpen} />
<LabelJump bind:open={labelJumpOpen} />
<LabelPicker />
<MovePicker />
<UnsyncedPanel bind:open={unsyncedOpen} />
<ComposeDock />
<Toaster />

<style>
	.skip {
		position: absolute;
		left: 12px;
		top: -60px;
		z-index: 100;
		padding: 8px 14px;
		border-radius: 8px;
		background: var(--accent);
		color: var(--accent-ink);
	}

	.skip:focus {
		top: 10px;
	}

	.shell {
		display: grid;
		grid-template-columns: var(--nav-w) minmax(0, 1fr);
		grid-template-rows: var(--topbar-h) minmax(0, 1fr);
		height: 100dvh;
	}

	.top {
		grid-column: 1 / -1;
		display: flex;
		align-items: center;
		gap: 16px;
		padding: 0 20px 0 12px;
	}

	.brand {
		width: calc(var(--nav-w) - 28px);
		padding-left: 12px;
	}

	.menu {
		display: none;
		place-items: center;
		width: 44px;
		height: 44px;
		border: 0;
		border-radius: 50%;
		background: none;
		cursor: pointer;
	}

	.menu:hover {
		background: var(--hover);
	}

	.unsynced {
		display: inline-flex;
		flex: none;
		align-items: center;
		gap: 6px;
		height: 32px;
		margin-left: auto;
		padding: 0 12px;
		border: 1px solid var(--line-strong);
		border-radius: 999px;
		background: var(--surface);
		color: var(--ink-2);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
	}

	.unsynced.failed {
		border-color: color-mix(in oklab, var(--danger) 50%, transparent);
		background: var(--danger-soft);
		color: var(--danger);
	}

	.unsynced + .live {
		margin-left: 0;
	}

	.live {
		width: 8px;
		height: 8px;
		margin-left: auto;
		border-radius: 50%;
		background: var(--faint);
	}

	.live.on {
		background: var(--ok);
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--ok) 20%, transparent);
	}

	.side {
		position: relative;
		min-height: 0;
	}

	/* z-index: main's children are layers (will-change), which would cover the handle. */
	.resize {
		position: absolute;
		inset-block: 0;
		inset-inline-end: -4px;
		z-index: 1;
		width: 8px;
		cursor: col-resize;
		touch-action: none;
	}

	.resize:hover,
	.resize:focus-visible {
		outline: none;
		background: linear-gradient(var(--accent), var(--accent)) center / 2px 100% no-repeat;
	}

	.scrim {
		display: none;
	}

	main {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
		margin: 0 16px 16px 0;
		overflow: hidden;
		border-radius: var(--radius-lg);
		border: 1px solid var(--line);
		background: var(--surface);
	}

	/* A blurred shadow on main cost WebKit ~100 ms per startup frame and was
	   repainted on every scroll frame; the border and this layer avoid both. */
	main > :global(*) {
		flex: 1;
		min-height: 0;
		will-change: transform;
	}

	main > .banners {
		flex: none;
		margin: 0;
		padding: 0;
		list-style: none;
		will-change: auto;
	}

	.banners li {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 9px 16px;
		border-bottom: 1px solid color-mix(in oklab, var(--danger) 30%, transparent);
		background: color-mix(in oklab, var(--danger) 12%, var(--surface));
		color: var(--ink);
		font-size: 13px;
	}

	.banners span {
		flex: 1;
	}

	.banners a {
		flex: none;
		color: var(--accent);
		font-weight: 600;
	}

	@media (pointer: coarse), (max-width: 720px) {
		.unsynced {
			height: 44px;
		}
	}

	@media (max-width: 720px) {
		.shell {
			grid-template-columns: minmax(0, 1fr);
			grid-template-rows: calc(64px + env(safe-area-inset-top)) minmax(0, 1fr);
			padding: 0 env(safe-area-inset-right) 0 env(safe-area-inset-left);
		}

		.selecting {
			grid-template-rows: minmax(0, 1fr);
		}

		.top[hidden] {
			display: none;
		}

		.top {
			gap: 6px;
			padding: env(safe-area-inset-top) 10px 0 4px;
		}

		.brand {
			display: none;
		}

		.resize {
			display: none;
		}

		.menu {
			display: grid;
		}

		.live {
			flex: none;
			margin: 0 4px;
		}

		.side {
			position: fixed;
			inset: 0 auto 0 0;
			z-index: 50;
			width: min(300px, 84vw);
			padding: calc(12px + env(safe-area-inset-top)) 0 env(safe-area-inset-bottom)
				env(safe-area-inset-left);
			background: var(--bg);
			box-shadow: var(--shadow-lg);
			transform: translateX(-102%);
			transition: transform 200ms var(--ease);
		}

		.drawer .side {
			transform: none;
		}

		.drawer .scrim {
			display: block;
			position: fixed;
			inset: 0;
			z-index: 40;
			border: 0;
			background: rgb(8 10 14 / 0.4);
		}

		main {
			margin: 0;
			padding-bottom: env(safe-area-inset-bottom);
			border: 0;
			border-radius: 0;
		}
	}
</style>
