<script lang="ts">
	import type Squire from 'squire-rte';
	import { onMount } from 'svelte';
	import Icon, { type IconName } from '../ui/Icon.svelte';
	import { sanitizeFragment } from './sanitize';

	let {
		html,
		label,
		onchange,
		onready
	}: {
		/** Initial content; later edits flow out through `onchange`. */
		html: string;
		label: string;
		onchange: (html: string, text: string) => void;
		/** The editor loaded and shows `html`. */
		onready?: () => void;
	} = $props();

	let root: HTMLDivElement;
	let editor: Squire | null = $state(null);
	let path = $state('');
	let linkOpen = $state(false);
	let linkUrl = $state('');

	interface Tool {
		icon?: IconName;
		text?: string;
		label: string;
		key?: string;
		active?: () => boolean;
		run: (e: Squire) => void;
	}

	const has = (tag: string) => () => editor?.hasFormat(tag) ?? false;
	const inPath = (tag: string) => () => path.split('>').includes(tag);

	const TOOLS: Tool[] = [
		{
			text: 'B',
			label: 'Bold',
			key: 'Ctrl+B',
			active: has('B'),
			run: (e) => (e.hasFormat('B') ? e.removeBold() : e.bold())
		},
		{
			text: 'I',
			label: 'Italic',
			key: 'Ctrl+I',
			active: has('I'),
			run: (e) => (e.hasFormat('I') ? e.removeItalic() : e.italic())
		},
		{
			text: 'U',
			label: 'Underline',
			key: 'Ctrl+U',
			active: has('U'),
			run: (e) => (e.hasFormat('U') ? e.removeUnderline() : e.underline())
		},
		{
			icon: 'link',
			label: 'Link',
			active: has('A'),
			run: (e) => {
				if (e.hasFormat('A')) e.removeLink();
				else {
					linkUrl = '';
					linkOpen = true;
				}
			}
		},
		{
			icon: 'listBullet',
			label: 'Bulleted list',
			active: inPath('UL'),
			run: (e) => (inPath('UL')() ? e.removeList() : e.makeUnorderedList())
		},
		{
			icon: 'listNumber',
			label: 'Numbered list',
			active: inPath('OL'),
			run: (e) => (inPath('OL')() ? e.removeList() : e.makeOrderedList())
		},
		{
			icon: 'quote',
			label: 'Quote',
			active: inPath('BLOCKQUOTE'),
			run: (e) => (inPath('BLOCKQUOTE')() ? e.decreaseQuoteLevel() : e.increaseQuoteLevel())
		},
		{ icon: 'clearFormat', label: 'Clear formatting', run: (e) => e.removeAllFormatting() }
	];

	onMount(() => {
		let instance: Squire | null = null;
		let cancelled = false;
		void (async () => {
			const [{ default: SquireEditor }, { default: purify }] = await Promise.all([
				import('squire-rte'),
				import('dompurify')
			]);
			if (cancelled) return;
			instance = new SquireEditor(root, {
				blockTag: 'DIV',
				sanitizeToDOMFragment: (dirty) => sanitizeFragment(purify, dirty),
				tagAttributes: { blockquote: { type: 'cite' }, li: {}, a: { rel: 'noopener noreferrer' } }
			});
			if (html) instance.setHTML(html);
			instance.addEventListener('input', () => {
				if (instance) onchange(instance.getHTML(), root.innerText);
			});
			instance.addEventListener('pathChange', () => {
				path = instance?.getPath() ?? '';
			});
			editor = instance;
			if (focusPending) focus();
			onready?.();
		})();
		return () => {
			cancelled = true;
			instance?.destroy();
		};
	});

	/** Focus asked for before the editor loaded; applied once it has. */
	let focusPending = false;

	export function focus(): void {
		if (!editor) {
			focusPending = true;
			return;
		}
		editor.focus();
		editor.moveCursorToStart();
	}

	/** The content as the editor serializes it; empty until it loaded. */
	export function getHTML(): string {
		return editor?.getHTML() ?? '';
	}

	/** Replaces the content; returns it as the editor serializes it. */
	export function setHTML(next: string): { html: string; text: string } {
		if (!editor) return { html: next, text: '' };
		editor.setHTML(next);
		return { html: editor.getHTML(), text: root.innerText };
	}

	function addLink(event: SubmitEvent) {
		event.preventDefault();
		const url = linkUrl.trim();
		if (editor && url) {
			editor.makeLink(/^[a-z][a-z0-9+.-]*:/i.test(url) ? url : `https://${url}`);
		}
		linkOpen = false;
		editor?.focus();
	}
</script>

<div class="editor">
	<div
		class="area"
		bind:this={root}
		role="textbox"
		tabindex="0"
		aria-multiline="true"
		aria-label={label}
		data-editor
	></div>
	<div class="tools" role="toolbar" aria-label="Formatting">
		{#each TOOLS as tool (tool.label)}
			<button
				type="button"
				class="tool"
				class:on={tool.active?.()}
				aria-label={tool.label}
				aria-pressed={tool.active ? tool.active() : undefined}
				title={tool.key ? `${tool.label} (${tool.key})` : tool.label}
				disabled={!editor}
				onmousedown={(e) => e.preventDefault()}
				onclick={() => {
					if (!editor) return;
					tool.run(editor);
					path = editor.getPath();
					editor.focus();
				}}
			>
				{#if tool.icon}<Icon name={tool.icon} size={17} />{:else}<span class="glyph {tool.text}">
						{tool.text}
					</span>{/if}
			</button>
		{/each}
		{#if linkOpen}
			<form class="link" onsubmit={addLink}>
				<input
					bind:value={linkUrl}
					placeholder="https://"
					aria-label="Link address"
					onkeydown={(e) => {
						if (e.key === 'Escape') {
							e.stopPropagation();
							linkOpen = false;
						}
					}}
				/>
				<button class="btn" type="submit">Add</button>
			</form>
		{/if}
	</div>
</div>

<style>
	.editor {
		display: flex;
		flex-direction: column;
		min-height: 0;
		flex: 1;
	}

	.area {
		flex: 1;
		min-height: 120px;
		padding: 12px 16px;
		overflow-y: auto;
		outline: none;
		line-height: 1.55;
		overflow-wrap: anywhere;
		cursor: text;
	}

	.area:focus-visible {
		outline: none;
	}

	.area :global(blockquote) {
		margin: 8px 0;
		padding-left: 12px;
		border-left: 3px solid var(--line-strong);
		color: var(--ink-2);
	}

	.area :global(ul),
	.area :global(ol) {
		margin: 4px 0;
		padding-left: 24px;
	}

	.area :global(a) {
		color: var(--accent);
	}

	.tools {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 2px;
		padding: 4px 10px;
		border-top: 1px solid var(--line);
	}

	.tool {
		display: grid;
		place-items: center;
		width: 32px;
		height: 30px;
		border: 0;
		border-radius: 7px;
		background: none;
		color: var(--ink-2);
		cursor: pointer;
	}

	.tool:hover:not(:disabled) {
		background: var(--hover);
	}

	.tool.on {
		background: var(--accent-soft);
		color: var(--accent);
	}

	.glyph {
		font-family: var(--font-mono);
		font-size: 14px;
		font-weight: 600;
	}

	.glyph.B {
		font-weight: 800;
	}

	.glyph.I {
		font-style: italic;
	}

	.glyph.U {
		text-decoration: underline;
	}

	.link {
		display: flex;
		gap: 6px;
		margin-left: 6px;
	}

	.link input {
		width: 200px;
		height: 30px;
		padding: 0 10px;
		border: 1px solid var(--line-strong);
		border-radius: 8px;
		background: var(--surface-2);
	}

	.link .btn {
		height: 30px;
	}
</style>
