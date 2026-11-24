<script lang="ts">
	import { isDesktop } from '../transport/tauri';
	import Icon from '../ui/Icon.svelte';
	import { connectivity } from './connectivity.svelte';

	const desktop = isDesktop();
</script>

{#if connectivity.offline && !desktop}
	<div class="offline" role="status" data-offline>
		<Icon name="alert" size={16} />
		<span>You're offline. Mail will load when you're back online.</span>
	</div>
{/if}

<style>
	.offline {
		display: flex;
		flex: none;
		align-items: center;
		gap: 10px;
		padding: 9px 16px;
		border-bottom: 1px solid var(--line);
		background: var(--surface-2);
		color: var(--ink);
		font-size: 13px;
	}
</style>
