<script lang="ts">
	import { accountHue, accountInitial } from '../stores';

	let { id, name, size = 18 }: { id: number; name: string; size?: number } = $props();
</script>

<span
	class="marker"
	style:--hue={accountHue(id)}
	style:--size="{size}px"
	title={name}
	aria-label="Account {name}"
	role="img"
>
	{accountInitial(name)}
</span>

<style>
	.marker {
		display: inline-grid;
		flex: none;
		place-items: center;
		width: var(--size);
		height: var(--size);
		border-radius: 50%;
		background: oklch(0.62 0.13 var(--hue));
		color: #fff;
		font-size: calc(var(--size) * 0.56);
		font-weight: 700;
		line-height: 1;
	}
</style>
