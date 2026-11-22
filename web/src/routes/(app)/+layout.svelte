<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { shortcuts } from '#lib/shortcuts/index.ts';
	import { hub, lists, nav, resetAppState } from '#lib/stores/index.ts';
	import { api } from '#lib/transport/index.ts';
	import HelpOverlay from '#lib/ui/HelpOverlay.svelte';
	import Icon from '#lib/ui/Icon.svelte';
	import LabelJump from '#lib/ui/LabelJump.svelte';
	import Logo from '#lib/ui/Logo.svelte';
	import NavRail from '#lib/ui/NavRail.svelte';
	import SearchBar from '#lib/ui/SearchBar.svelte';
	import { onMount } from 'svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();

	let search: SearchBar | undefined = $state();
	let helpOpen = $state(false);
	let labelJumpOpen = $state(false);
	let drawerOpen = $state(false);

	const query = $derived(
		page.url.pathname === '/search' ? (page.url.searchParams.get('q') ?? '') : ''
	);

	onMount(() => {
		api.onUnauthorized = (error) => {
			resetAppState();
			const next = encodeURIComponent(location.pathname + location.search);
			void goto(error.code === 'setup_required' ? '/setup' : `/login?next=${next}`);
		};
		void nav.load();
		return shortcuts.push((action) => {
			switch (action) {
				case 'search':
					search?.focus();
					return true;
				case 'help':
					helpOpen = !helpOpen;
					return true;
				case 'goInbox':
					void goto('/inbox');
					return true;
				case 'goLabel':
					labelJumpOpen = true;
					return true;
				case 'back':
					if (page.url.pathname.startsWith('/thread/')) {
						void goto(lists.currentPath);
						return true;
					}
					return false;
				case 'escape':
					if (drawerOpen) {
						drawerOpen = false;
						return true;
					}
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

<a class="skip" href="#main">Skip to conversations</a>
<div class="shell" class:drawer={drawerOpen}>
	<header class="top">
		<button
			class="menu"
			aria-label="Menu"
			aria-expanded={drawerOpen}
			aria-controls="nav"
			onclick={() => (drawerOpen = !drawerOpen)}
		>
			<Icon name="menu" />
		</button>
		<div class="brand"><Logo /></div>
		<SearchBar bind:this={search} {query} />
		<span
			class="live"
			class:on={hub.connected}
			role="status"
			title={hub.connected ? 'Live updates on' : 'Live updates paused'}
		>
			<span class="sr-only">{hub.connected ? 'Live updates on' : 'Live updates paused'}</span>
		</span>
	</header>
	<div class="side" id="nav">
		<NavRail
			open={drawerOpen}
			onnavigate={() => (drawerOpen = false)}
			onhelp={() => (helpOpen = true)}
			onlogout={logout}
		/>
	</div>
	<button class="scrim" tabindex="-1" aria-label="Close menu" onclick={() => (drawerOpen = false)}
	></button>
	<main id="main">
		{@render children()}
	</main>
</div>

<HelpOverlay bind:open={helpOpen} />
<LabelJump bind:open={labelJumpOpen} />

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
		min-height: 0;
	}

	.scrim {
		display: none;
	}

	main {
		min-width: 0;
		min-height: 0;
		margin: 0 16px 16px 0;
		overflow: hidden;
		border-radius: var(--radius-lg);
		background: var(--surface);
		box-shadow: var(--shadow);
	}

	@media (max-width: 720px) {
		.shell {
			grid-template-columns: minmax(0, 1fr);
			grid-template-rows: 64px minmax(0, 1fr);
		}

		.top {
			gap: 6px;
			padding: 0 10px 0 4px;
		}

		.brand {
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
			padding-top: 12px;
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
			border-radius: 0;
			box-shadow: none;
		}
	}
</style>
