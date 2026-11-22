<script lang="ts">
	import { encode } from 'uqr';

	let { value, label }: { value: string; label: string } = $props();

	const QUIET = 2;
	const qr = $derived(encode(value, { ecc: 'M', border: QUIET }));
	const path = $derived.by(() => {
		let d = '';
		qr.data.forEach((row, y) =>
			row.forEach((dark, x) => {
				if (dark) d += `M${x} ${y}h1v1h-1z`;
			})
		);
		return d;
	});
</script>

<svg
	class="qr"
	viewBox="0 0 {qr.size} {qr.size}"
	role="img"
	aria-label={label}
	shape-rendering="crispEdges"
>
	<rect width={qr.size} height={qr.size} fill="#fff" />
	<path d={path} fill="#14171c" />
</svg>

<style>
	.qr {
		display: block;
		width: 184px;
		height: 184px;
		border-radius: 12px;
	}
</style>
