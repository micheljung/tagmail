<script lang="ts">
	import '../app.css';
	import { dev } from '$app/env';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import favicon from '#lib/assets/favicon.svg';
	import { composer } from '#lib/compose/composer.svelte.ts';
	import { desktop } from '#lib/desktop/desktop.svelte.ts';
	import { connectivity } from '#lib/pwa/connectivity.svelte.ts';
	import { registerServiceWorker, shouldRegister } from '#lib/pwa/register.ts';
	import { pushEnv, syncPush, unsubscribeQuietly } from '#lib/push/client.ts';
	import { theme, toasts } from '#lib/stores/index.ts';
	import { TauriTransport, api } from '#lib/transport/index.ts';
	import PairConfirm from '#lib/ui/PairConfirm.svelte';
	import { onMount } from 'svelte';
	import type { CurrentAuthDto } from '@tagmail/api-types';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
	let isDesktop = $state(false);
	let registration = $state<Promise<ServiceWorkerRegistration> | null>(null);
	let pushSyncedFor: string | null = null;

	function confirmPair() {
		if (desktop.confirmRepair()) void goto('/pair');
	}

	function startPwa() {
		const supported = 'serviceWorker' in navigator;
		if (!shouldRegister({ dev, desktop: false, secure: isSecureContext, supported })) return;
		registration = registerServiceWorker(navigator.serviceWorker, {
			onUpdate: (apply) =>
				toasts.show('A new version of Tagmail is available', {
					key: 'pwa-update',
					duration: 0,
					actions: [{ label: 'Reload', run: apply }]
				}),
			onNavigate: (path) => void goto(path)
		});
		registration.catch((error: unknown) =>
			console.warn('service worker registration failed', error)
		);
		api.beforeLogout = () =>
			unsubscribeQuietly(async () => (await registration)?.pushManager ?? null);
	}

	$effect(() => {
		const auth = page.data.session as CurrentAuthDto | null | undefined;
		const id = auth?.via === 'session' ? auth.session?.id : undefined;
		if (!registration || !id || id === pushSyncedFor) return;
		if (pushEnv().permission !== 'granted') return;
		pushSyncedFor = id;
		void registration
			.then((r) => syncPush(r.pushManager, api))
			.catch((error: unknown) => console.warn('push subscription sync failed', error));
	});

	onMount(() => {
		theme.init();
		const transport = api.transport;
		isDesktop = transport instanceof TauriTransport;
		if (!(transport instanceof TauriTransport)) {
			startPwa();
			return connectivity.watch();
		}
		const stop = desktop.start(api, (event, handler) => transport.listen(event, handler), {
			mailto: (draft) => composer.open(draft),
			pair: () => void goto('/pair')
		});
		return () => void stop.then((unlisten) => unlisten());
	});
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
</svelte:head>

{@render children()}
{#if isDesktop}<PairConfirm onconfirm={confirmPair} />{/if}
