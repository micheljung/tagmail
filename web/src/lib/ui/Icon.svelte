<script lang="ts" module>
	const PATHS = {
		inbox:
			'M4 13h4l1.5 3h5L16 13h4M4 13l2.4-7.2A1.2 1.2 0 0 1 7.5 5h9a1.2 1.2 0 0 1 1.1.8L20 13v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1z',
		star: 'm12 3.6 2.6 5.3 5.8.8-4.2 4.1 1 5.8L12 16.9l-5.2 2.7 1-5.8-4.2-4.1 5.8-.8z',
		send: 'M4.5 12 20 4.5 15.5 20l-3.2-6.3zM12.3 13.7 20 4.5',
		tag: 'M3.8 12.6 11.4 5h7.1a.5.5 0 0 1 .5.5v7.1l-7.6 7.6a1 1 0 0 1-1.4 0l-6.2-6.2a1 1 0 0 1 0-1.4zM15.2 8.8h.01',
		search: 'M10.8 17.6a6.8 6.8 0 1 0 0-13.6 6.8 6.8 0 0 0 0 13.6zM20 20l-4.4-4.4',
		menu: 'M4 7h16M4 12h16M4 17h16',
		settings:
			'M12 15.2a3.2 3.2 0 1 0 0-6.4 3.2 3.2 0 0 0 0 6.4zM19.4 13.5l1.6 1.2-1.8 3.2-1.9-.7a7.4 7.4 0 0 1-1.9 1.1L15 20.4h-3.6l-.4-2.1a7.4 7.4 0 0 1-1.9-1.1l-1.9.7-1.8-3.2 1.6-1.2a7.6 7.6 0 0 1 0-2.2L5.4 10l1.8-3.2 1.9.7A7.4 7.4 0 0 1 11 6.4l.4-2.1H15l.4 2.1a7.4 7.4 0 0 1 1.9 1.1l1.9-.7 1.8 3.2-1.6 1.2a7.6 7.6 0 0 1 0 2.3z',
		back: 'M19 12H5m6-6-6 6 6 6',
		clip: 'm20 11.3-7.6 7.6a4.6 4.6 0 0 1-6.5-6.5l8-8a3 3 0 0 1 4.3 4.3l-8 8a1.5 1.5 0 0 1-2.1-2.1l7.3-7.3',
		image:
			'M5 4h14a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zM4 16l5-5 4 4 2.5-2.5L20 17M15.5 8.5h.01',
		keyboard:
			'M3 6.5h18a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1v-9a1 1 0 0 1 1-1zM6 10h.01M9.5 10h.01M13 10h.01M16.5 10h.01M7 14h10',
		sun: 'M12 16.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9zM12 2v2m0 16v2M4.9 4.9l1.4 1.4m11.4 11.4 1.4 1.4M2 12h2m16 0h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
		moon: 'M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z',
		logout: 'M15 4h3a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-3M10 16l4-4-4-4m4 4H4',
		devices:
			'M4 6a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v8H4zM2 17h12M17 10h4a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1h-4a1 1 0 0 1-1-1v-8a1 1 0 0 1 1-1z',
		close: 'M6 6l12 12M18 6 6 18',
		up: 'm6 15 6-6 6 6',
		down: 'm6 9 6 6 6-6',
		shield: 'M12 3 4.5 6v5.5c0 4.4 3.2 8.2 7.5 9.5 4.3-1.3 7.5-5.1 7.5-9.5V6z',
		check: 'm5 12.5 4.5 4.5L19 7.5',
		plus: 'M12 5v14M5 12h14',
		archive: 'M4 5h16v4H4zM5.5 9v9.5a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1V9M10 13h4',
		trash:
			'M4.5 6.5h15M9.5 6.5V4.8a.8.8 0 0 1 .8-.8h3.4a.8.8 0 0 1 .8.8v1.7M6.5 6.5l.9 12.6a1 1 0 0 0 1 .9h7.2a1 1 0 0 0 1-.9l.9-12.6',
		folder:
			'M3.5 7a1 1 0 0 1 1-1h4.6l2 2h8.4a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1h-15a1 1 0 0 1-1-1zM12 11.5l2.5 2.5-2.5 2.5M9 14h5.3',
		read: 'M3.5 9.5 12 4l8.5 5.5V19a1 1 0 0 1-1 1h-15a1 1 0 0 1-1-1zM3.5 9.5l8.5 5.5 8.5-5.5',
		unread: 'M3.5 6h17v12a1 1 0 0 1-1 1h-15a1 1 0 0 1-1-1zM3.5 6.5l8.5 6.5 8.5-6.5',
		reply: 'M10 7 5 12l5 5M5 12h9a5 5 0 0 1 5 5v1',
		replyAll: 'M8.5 7l-5 5 5 5M13 7l-5 5 5 5M8 12h7a5 5 0 0 1 5 5v1',
		forward: 'M14 7l5 5-5 5M19 12h-9a5 5 0 0 0-5 5v1',
		pencil: 'M4 20h4L19 9a2.8 2.8 0 0 0-4-4L4 16zM13.5 6.5l4 4',
		sync: 'M19.5 12a7.5 7.5 0 0 1-13.1 5M4.5 12A7.5 7.5 0 0 1 17.6 7M17.8 3.5v3.7h-3.7M6.2 20.5v-3.7h3.7',
		minimize: 'M6 18h12',
		expand: 'M14 4h6v6M10 20H4v-6M20 4l-7 7M4 20l7-7',
		bell: 'M6 16.5V11a6 6 0 0 1 12 0v5.5l1.5 1.5h-15zM10 20.5a2 2 0 0 0 4 0',
		calendar:
			'M5 5.5h14a1 1 0 0 1 1 1V19a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V6.5a1 1 0 0 1 1-1zM4 10h16M8.5 3.5v4M15.5 3.5v4',
		alert: 'M12 4 2.8 19.5h18.4zM12 10v4.5M12 17.2h.01',
		link: 'M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1',
		listBullet: 'M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01',
		listNumber:
			'M10 6h10M10 12h10M10 18h10M4 5l1.5-1v5M3.8 14.2a1.4 1.4 0 0 1 2.6.7c0 1-2.6 2.1-2.6 3.1h2.7',
		quote:
			'M6 17c-1.5 0-2.5-1.2-2.5-3 0-3 1.8-5.5 4.5-7M15 17c-1.5 0-2.5-1.2-2.5-3 0-3 1.8-5.5 4.5-7',
		clearFormat: 'M7 5h11M12.5 5 9 19M4 4l16 16',
		account: 'M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM4.5 20a7.5 7.5 0 0 1 15 0'
	} as const;

	export type IconName = keyof typeof PATHS;
</script>

<script lang="ts">
	let {
		name,
		size = 20,
		filled = false,
		label
	}: { name: IconName; size?: number; filled?: boolean; label?: string } = $props();
</script>

<svg
	class="icon"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	fill={filled ? 'currentColor' : 'none'}
	stroke="currentColor"
	stroke-width="1.7"
	stroke-linecap="round"
	stroke-linejoin="round"
	role={label ? 'img' : undefined}
	aria-label={label}
	aria-hidden={label ? undefined : 'true'}
>
	<path d={PATHS[name]} />
</svg>

<style>
	.icon {
		flex: none;
		display: block;
	}
</style>
