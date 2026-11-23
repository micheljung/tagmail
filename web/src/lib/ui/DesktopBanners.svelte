<script lang="ts">
	import { DOWNLOAD_URL, desktop } from '../desktop/desktop.svelte';
	import Icon from './Icon.svelte';

	const version = (v: { major: number; minor: number }) => `${v.major}.${v.minor}`;
</script>

{#if desktop.updateRequired || desktop.progress !== null || desktop.offlineText || desktop.status?.warnings.length}
	<ul class="banners">
		{#if desktop.updateRequired}
			<li class="blocking" role="alert" data-desktop="update">
				<Icon name="alert" size={16} />
				<span>
					Update Tagmail to keep syncing: your server needs app version
					{version(desktop.updateRequired.minClient)} or newer (server
					{version(desktop.updateRequired.server)}). Mail on this device stays readable.
				</span>
				<a href={DOWNLOAD_URL} target="_blank" rel="noreferrer">Download</a>
			</li>
		{/if}
		{#if desktop.progress !== null}
			<li role="status" data-desktop="bootstrap">
				<Icon name="sync" size={16} />
				<span>Downloading your mail to this device… {desktop.progress}%</span>
				<progress max="100" value={desktop.progress}></progress>
			</li>
		{/if}
		{#if desktop.offlineText}
			<li role="status" data-desktop="offline">
				<Icon name="alert" size={16} />
				<span>{desktop.offlineText}</span>
			</li>
		{/if}
		{#each desktop.status?.warnings ?? [] as warning (warning)}
			<li role="status" data-desktop="warning">
				<Icon name="alert" size={16} />
				<span>{warning}</span>
			</li>
		{/each}
	</ul>
{/if}

<style>
	.banners {
		flex: none;
		margin: 0;
		padding: 0;
		list-style: none;
	}

	li {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 9px 16px;
		border-bottom: 1px solid var(--line);
		background: var(--surface-2);
		color: var(--ink);
		font-size: 13px;
	}

	li.blocking {
		border-bottom-color: color-mix(in oklab, var(--danger) 30%, transparent);
		background: color-mix(in oklab, var(--danger) 12%, var(--surface));
	}

	span {
		flex: 1;
	}

	a {
		flex: none;
		color: var(--accent);
		font-weight: 600;
	}

	progress {
		flex: none;
		width: 120px;
	}
</style>
