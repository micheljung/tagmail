<script lang="ts">
	import { APP_NAME } from '#lib/document-title.ts';
	import { labelColor } from '#lib/format.ts';
	import { findByName } from '#lib/ops/labels.ts';
	import { nav, toasts } from '#lib/stores/index.ts';
	import { api, type LabelDto, type LabelJobDto } from '#lib/transport/index.ts';
	import { errorText } from '#lib/ui/errors.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import { onMount } from 'svelte';
	import { SvelteMap } from 'svelte/reactivity';

	/** Swatches picked to stay legible as chips in light and dark themes. */
	const PALETTE = [
		'#d6453d',
		'#e8833a',
		'#d9a51c',
		'#3d9a50',
		'#1f9c95',
		'#2f7fd6',
		'#4f5bd5',
		'#8a4fd6',
		'#c2468f',
		'#7a7f87'
	];
	const JOB_POLL_MS = 600;

	let newName = $state('');
	let newColor = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let error = $state<string | null>(null);
	let editing = $state<string | null>(null);
	let draftName = $state('');
	let colorFor = $state<string | null>(null);
	let confirmDelete = $state<string | null>(null);
	const jobs = new SvelteMap<string, LabelJobDto>();
	let polling: ReturnType<typeof setTimeout> | null = null;

	onMount(() => {
		void nav.load();
		void pollJobs();
		return () => {
			if (polling) clearTimeout(polling);
		};
	});

	async function pollJobs() {
		polling = null;
		try {
			const list = await api.labelJobs();
			jobs.clear();
			for (const job of list) if (job.status === 'running') jobs.set(job.label, job);
			if (jobs.size > 0) polling = setTimeout(() => void pollJobs(), JOB_POLL_MS);
			else await nav.load();
		} catch {
			jobs.clear();
		}
	}

	function watchJobs() {
		if (!polling) polling = setTimeout(() => void pollJobs(), JOB_POLL_MS);
	}

	async function create(event: SubmitEvent) {
		event.preventDefault();
		const name = newName.trim();
		if (!name) return;
		if (findByName(nav.labels, name)) {
			error = `A label called “${name}” already exists.`;
			return;
		}
		busy = 'create';
		error = null;
		try {
			const label = await api.createLabel(name, newColor);
			await nav.load();
			newName = '';
			newColor = null;
			toasts.show(`Created label “${label.name}”`);
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = null;
		}
	}

	function startRename(label: LabelDto) {
		editing = label.key;
		draftName = label.name;
		requestAnimationFrame(() =>
			document.querySelector<HTMLInputElement>(`[data-rename="${CSS.escape(label.key)}"]`)?.select()
		);
	}

	async function rename(label: LabelDto) {
		const name = draftName.trim();
		editing = null;
		if (!name || name === label.name) return;
		busy = label.key;
		error = null;
		try {
			const { label: renamed, jobId } = await api.updateLabel(label.key, { name });
			if (jobId != null) {
				jobs.set(renamed.key, {
					id: jobId,
					label: renamed.key,
					kind: 'rename',
					done: 0,
					total: 0,
					status: 'running'
				});
				watchJobs();
			}
			await nav.load();
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = null;
		}
	}

	async function recolor(label: LabelDto, color: string | null) {
		colorFor = null;
		busy = label.key;
		try {
			await api.updateLabel(label.key, { color });
			await nav.load();
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = null;
		}
	}

	async function remove(label: LabelDto) {
		confirmDelete = null;
		busy = label.key;
		error = null;
		try {
			await api.deleteLabel(label.key);
			await nav.load();
			watchJobs();
			toasts.show(`Deleted label “${label.name}”`);
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = null;
		}
	}

	function progress(job: LabelJobDto): string {
		if (job.total === 0) return 'Starting…';
		return `${Math.round((job.done / job.total) * 100)}%`;
	}
</script>

<svelte:head>
	<title>Labels · {APP_NAME}</title>
</svelte:head>

<div class="settings">
	<h1 tabindex="-1">Labels</h1>
	<p class="lead">
		Labels are IMAP keywords, so they follow your mail into every client that shows them.
	</p>

	{#if error}<p class="error" role="alert">{error}</p>{/if}

	<section aria-labelledby="new-title">
		<h2 id="new-title">New label</h2>
		<form class="add" onsubmit={create}>
			<input
				bind:value={newName}
				placeholder="Label name"
				aria-label="New label name"
				maxlength="120"
			/>
			<div class="swatches" role="radiogroup" aria-label="New label color">
				<button
					type="button"
					class="swatch auto"
					role="radio"
					aria-checked={newColor === null}
					aria-label="Automatic color"
					onclick={() => (newColor = null)}
				></button>
				{#each PALETTE as c (c)}
					<button
						type="button"
						class="swatch"
						role="radio"
						aria-checked={newColor === c}
						aria-label="Color {c}"
						style:--c={c}
						onclick={() => (newColor = c)}
					></button>
				{/each}
			</div>
			<button class="btn primary" type="submit" disabled={busy === 'create' || !newName.trim()}>
				<Icon name="plus" size={16} />Create
			</button>
		</form>
	</section>

	<section aria-labelledby="labels-title">
		<h2 id="labels-title">Your labels</h2>
		{#if !nav.loaded}
			<p class="muted">Loading…</p>
		{:else if nav.labels.length === 0}
			<p class="muted">No labels yet. Create one above or press <kbd>l</kbd> on a conversation.</p>
		{:else}
			<ul class="items">
				{#each nav.labels as label (label.key)}
					{@const job = jobs.get(label.key)}
					<li data-label={label.key}>
						<div class="pick">
							<button
								class="tag"
								style:--c={labelColor(label.key, label.color)}
								aria-label="Color of {label.name}"
								aria-expanded={colorFor === label.key}
								onclick={() => (colorFor = colorFor === label.key ? null : label.key)}
							></button>
							{#if colorFor === label.key}
								<div class="pop" role="radiogroup" aria-label="Color of {label.name}">
									<button
										type="button"
										class="swatch auto"
										role="radio"
										aria-checked={label.color === null}
										aria-label="Automatic color"
										onclick={() => recolor(label, null)}
									></button>
									{#each PALETTE as c (c)}
										<button
											type="button"
											class="swatch"
											role="radio"
											aria-checked={label.color === c}
											aria-label="Color {c}"
											style:--c={c}
											onclick={() => recolor(label, c)}
										></button>
									{/each}
								</div>
							{/if}
						</div>
						<div class="what">
							{#if editing === label.key}
								<input
									class="rename"
									data-rename={label.key}
									bind:value={draftName}
									aria-label="New name for {label.name}"
									onkeydown={(e) => {
										if (e.key === 'Enter') void rename(label);
										else if (e.key === 'Escape') {
											e.stopPropagation();
											editing = null;
										}
									}}
									onblur={() => void rename(label)}
								/>
							{:else}
								<strong>{label.name}</strong>
							{/if}
							{#if job}
								<span class="job" role="status">
									{job.kind === 'rename' ? 'Renaming' : 'Deleting'} on the server · {progress(job)}
									<progress max={job.total || 1} value={job.done} aria-label="Progress"></progress>
								</span>
							{/if}
						</div>
						{#if confirmDelete === label.key}
							<span class="confirm">Remove it from every conversation?</span>
							<button class="btn danger" onclick={() => remove(label)}>Delete</button>
							<button class="btn" onclick={() => (confirmDelete = null)}>Keep</button>
						{:else}
							<button
								class="btn"
								onclick={() => startRename(label)}
								disabled={busy === label.key || !!job}
							>
								Rename
							</button>
							<button
								class="btn danger"
								onclick={() => (confirmDelete = label.key)}
								disabled={busy === label.key || !!job}
							>
								Delete
							</button>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	</section>
</div>

<style>
	.settings {
		height: 100%;
		padding: 28px 36px 48px;
		overflow-y: auto;
	}

	h1 {
		margin: 0;
		font-size: 24px;
		font-weight: 600;
		letter-spacing: -0.015em;
		outline: none;
	}

	.lead {
		margin: 6px 0 26px;
		color: var(--muted);
	}

	section {
		max-width: 820px;
		margin-bottom: 30px;
	}

	h2 {
		margin: 0 0 10px;
		font-size: 15px;
		font-weight: 650;
	}

	.add {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px;
	}

	.add input,
	.rename {
		width: min(280px, 100%);
		height: 36px;
		padding: 0 12px;
		border: 1px solid var(--line-strong);
		border-radius: 999px;
		background: var(--surface-2);
	}

	.swatches,
	.pop {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}

	.swatch {
		width: 22px;
		height: 22px;
		padding: 0;
		border: 2px solid transparent;
		border-radius: 50%;
		background: var(--c);
		box-shadow: inset 0 0 0 2px var(--surface);
		cursor: pointer;
	}

	.swatch.auto {
		background: conic-gradient(#d6453d, #d9a51c, #3d9a50, #2f7fd6, #8a4fd6, #d6453d);
	}

	.swatch[aria-checked='true'] {
		border-color: var(--ink);
	}

	.items {
		margin: 0;
		padding: 0;
		border: 1px solid var(--line);
		border-radius: var(--radius);
		list-style: none;
	}

	.items li {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 12px 16px;
		border-bottom: 1px solid var(--line);
	}

	.items li:last-child {
		border-bottom: 0;
	}

	.pick {
		position: relative;
	}

	.tag {
		display: block;
		width: 26px;
		height: 18px;
		padding: 0;
		border: 0;
		clip-path: polygon(7px 0, 100% 0, 100% 100%, 7px 100%, 0 50%);
		border-radius: 0 4px 4px 0;
		background: var(--c);
		cursor: pointer;
	}

	.pop {
		position: absolute;
		top: 28px;
		left: -8px;
		z-index: 5;
		width: 168px;
		padding: 10px;
		border: 1px solid var(--line);
		border-radius: 12px;
		background: var(--surface-raised);
		box-shadow: var(--shadow-lg);
	}

	.what {
		display: grid;
		flex: 1;
		gap: 4px;
		min-width: 0;
	}

	.what strong {
		overflow: hidden;
		font-weight: 600;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.job {
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--muted);
		font-size: 12.5px;
	}

	.job progress {
		width: 120px;
		height: 4px;
		accent-color: var(--accent);
	}

	.confirm {
		color: var(--danger);
		font-size: 13px;
	}

	.muted {
		color: var(--muted);
	}

	.error {
		max-width: 820px;
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
	}

	@media (max-width: 720px) {
		.settings {
			padding: 20px 16px 40px;
		}

		.items li {
			flex-wrap: wrap;
		}
	}
</style>
