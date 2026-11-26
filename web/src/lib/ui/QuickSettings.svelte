<script lang="ts">
	import { tick } from 'svelte';
	import type { Density, InboxType, ReadingPane } from '../prefs';
	import { prefs, theme, type ThemeChoice } from '../stores';
	import Icon from './Icon.svelte';

	const uid = $props.id();
	const panelId = `${uid}-panel`;

	let open = $state(false);
	let root: HTMLDivElement;
	let gear: HTMLButtonElement;

	const DENSITY: { value: Density; label: string }[] = [
		{ value: 'default', label: 'Default' },
		{ value: 'comfortable', label: 'Comfortable' },
		{ value: 'compact', label: 'Compact' }
	];
	const THEME: { value: ThemeChoice; label: string }[] = [
		{ value: 'light', label: 'Light' },
		{ value: 'dark', label: 'Dark' },
		{ value: 'system', label: 'System' }
	];
	const INBOX_TYPE: { value: InboxType; label: string }[] = [
		{ value: 'default', label: 'Default' },
		{ value: 'unread', label: 'Unread first' }
	];
	const READING_PANE: { value: ReadingPane; label: string }[] = [
		{ value: 'none', label: 'No split' },
		{ value: 'right', label: 'Right of list' },
		{ value: 'below', label: 'Below list' }
	];

	async function close(focusGear: boolean) {
		open = false;
		if (!focusGear) return;
		await tick();
		gear.focus();
	}

	/** While open, keys stay here: Escape closes, and no global shortcut sees the rest. */
	function onkeydown(e: KeyboardEvent) {
		if (!open) return;
		if (e.key === 'Escape') {
			e.preventDefault();
			e.stopPropagation();
			void close(true);
		} else if (e.key !== 'Tab') {
			e.stopPropagation();
		}
	}

	function onpointerdown(e: PointerEvent) {
		if (!open || root.contains(e.target as Node)) return;
		void close(root.contains(document.activeElement));
	}
</script>

<svelte:document {onpointerdown} />

<div class="quick" data-quick-settings bind:this={root} {onkeydown} role="presentation">
	<button
		class="gear"
		aria-label="Quick settings"
		title="Quick settings"
		aria-expanded={open}
		aria-controls={panelId}
		bind:this={gear}
		onclick={() => (open = !open)}
	>
		<Icon name="settings" />
	</button>
	<div class="panel" id={panelId} role="group" aria-label="Quick settings" hidden={!open}>
		<h2>Quick settings</h2>
		<fieldset>
			<legend>Density</legend>
			{#each DENSITY as o (o.value)}
				<label>
					<input
						type="radio"
						name="{uid}-density"
						value={o.value}
						checked={prefs.density === o.value}
						onchange={() => prefs.setDensity(o.value)}
					/>
					{o.label}
				</label>
			{/each}
		</fieldset>
		<fieldset>
			<legend>Theme</legend>
			{#each THEME as o (o.value)}
				<label>
					<input
						type="radio"
						name="{uid}-theme"
						value={o.value}
						checked={theme.choice === o.value}
						onchange={() => theme.apply(o.value)}
					/>
					{o.label}
				</label>
			{/each}
		</fieldset>
		<fieldset>
			<legend>Inbox type</legend>
			{#each INBOX_TYPE as o (o.value)}
				<label>
					<input
						type="radio"
						name="{uid}-inbox"
						value={o.value}
						checked={prefs.inboxType === o.value}
						onchange={() => prefs.setInboxType(o.value)}
					/>
					{o.label}
				</label>
			{/each}
		</fieldset>
		<fieldset>
			<legend>Reading pane</legend>
			{#each READING_PANE as o (o.value)}
				<label>
					<input
						type="radio"
						name="{uid}-pane"
						value={o.value}
						checked={prefs.readingPane === o.value}
						onchange={() => prefs.setReadingPane(o.value)}
					/>
					{o.label}
				</label>
			{/each}
			{#if prefs.phone}<p class="hint">Ignored on phones</p>{/if}
		</fieldset>
		<a class="all" href="/settings" onclick={() => void close(false)}>See all settings</a>
	</div>
</div>

<style>
	.quick {
		position: relative;
		flex: none;
	}

	.gear {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: var(--ink-2);
		cursor: pointer;
	}

	.gear:hover,
	.gear[aria-expanded='true'] {
		background: var(--hover);
		color: var(--ink);
	}

	.panel {
		position: absolute;
		top: calc(100% + 8px);
		right: 0;
		z-index: 30;
		width: min(280px, calc(100vw - 20px));
		max-height: calc(100dvh - var(--topbar-h) - 16px);
		overflow-y: auto;
		padding: 14px 16px 16px;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	.panel[hidden] {
		display: none;
	}

	h2 {
		margin: 0 0 4px;
		font-size: 15px;
		font-weight: 650;
	}

	fieldset {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 10px 0;
		border: 0;
		border-bottom: 1px solid var(--line);
	}

	legend {
		margin-bottom: 4px;
		padding: 0;
		color: var(--muted);
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.02em;
	}

	label {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 32px;
		cursor: pointer;
	}

	input {
		margin: 0;
		accent-color: var(--accent);
	}

	.hint {
		margin: 2px 0 0;
		color: var(--muted);
		font-size: 12px;
	}

	.all {
		display: inline-block;
		margin-top: 12px;
		color: var(--accent);
		font-weight: 600;
	}

	@media (pointer: coarse), (max-width: 720px) {
		.gear {
			width: 44px;
			height: 44px;
		}

		label {
			min-height: 44px;
		}
	}
</style>
