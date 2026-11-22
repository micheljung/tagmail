<script lang="ts">
	import { labelHue } from '../format';

	let { labelKey, name }: { labelKey: string; name: string } = $props();
</script>

<span class="chip" style:--h={labelHue(labelKey)} title={name}>{name}</span>

<style>
	.chip {
		--notch: 7px;
		position: relative;
		display: inline-flex;
		align-items: center;
		flex: none;
		max-width: 14ch;
		height: 18px;
		padding: 0 7px 0 calc(var(--notch) + 6px);
		overflow: hidden;
		border-radius: 0 4px 4px 0;
		clip-path: polygon(var(--notch) 0, 100% 0, 100% 100%, var(--notch) 100%, 0 50%);
		background: oklch(var(--chip-l) var(--chip-c) var(--h));
		color: oklch(var(--chip-ink-l) var(--chip-ink-c) var(--h));
		font-size: 11.5px;
		font-weight: 600;
		letter-spacing: 0.01em;
		line-height: 1;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.chip::before {
		content: '';
		position: absolute;
		left: calc(var(--notch) - 1px);
		top: 50%;
		width: 4px;
		height: 4px;
		margin-top: -2px;
		border-radius: 50%;
		background: var(--row-bg, var(--surface));
	}
</style>
