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
		shield: 'M12 3 4.5 6v5.5c0 4.4 3.2 8.2 7.5 9.5 4.3-1.3 7.5-5.1 7.5-9.5V6z'
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
