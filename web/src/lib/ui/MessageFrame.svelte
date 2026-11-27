<script lang="ts">
	import { renderUrl } from '../transport';
	import { fitScale } from './fit';

	let { messageId, images, title }: { messageId: number; images: boolean; title: string } =
		$props();

	const MIN_HEIGHT = 48;
	/** Measure passes after widening the frame; content may grow once more. */
	const FIT_PASSES = 2;

	let paper: HTMLDivElement;
	let frame: HTMLIFrameElement;
	let loaded = $state(false);
	let scale = 1;
	/** `.paper` width (`clientWidth`, as AC13 measures it) the current scale was fitted to. */
	let fittedWidth = -1;
	let heightObserver: ResizeObserver | null = null;
	let widthObserver: ResizeObserver | null = null;

	function naturalWidth(doc: Document): number {
		return Math.max(doc.documentElement.scrollWidth, doc.body?.scrollWidth ?? 0);
	}

	/**
	 * The iframe keeps the unscaled content height so its document never
	 * scrolls; `.paper` takes the scaled one, as `transform` leaves layout alone.
	 */
	function measureHeight() {
		const doc = frame.contentDocument;
		if (!doc?.documentElement) return;
		const h = Math.max(
			MIN_HEIGHT,
			Math.ceil(Math.max(doc.documentElement.scrollHeight, doc.body?.scrollHeight ?? 0))
		);
		frame.style.height = `${h}px`;
		paper.style.height = `${Math.ceil(h * scale)}px`;
	}

	/**
	 * Lays the message out at the container width so author media queries see
	 * it, and scales the iframe down only when the content still overflows.
	 */
	function fit() {
		const doc = frame.contentDocument;
		if (!doc?.documentElement) return;
		const avail = paper.clientWidth;
		fittedWidth = avail;
		// From the minimum, so a refit at a wider width (a rotation) can shrink:
		// the document is never shorter than the iframe it fills.
		frame.style.height = `${MIN_HEIGHT}px`;
		frame.style.width = '';
		frame.style.transform = '';
		let natural = naturalWidth(doc);
		let s = fitScale(natural, avail);
		for (let pass = 0; pass < FIT_PASSES && s < 1; pass++) {
			frame.style.width = `${natural}px`;
			const grown = naturalWidth(doc);
			if (grown <= natural) break;
			natural = grown;
			s = fitScale(natural, avail);
		}
		if (s < 1) {
			frame.style.width = `${natural}px`;
			frame.style.transform = `scale(${s})`;
		} else {
			frame.style.width = '';
		}
		scale = s;
		paper.dataset.scale = String(s);
		measureHeight();
	}

	function disconnect() {
		heightObserver?.disconnect();
		widthObserver?.disconnect();
		heightObserver = widthObserver = null;
	}

	function onload() {
		disconnect();
		loaded = true;
		const doc = frame.contentDocument;
		if (!doc?.documentElement) return;
		fit();
		heightObserver = new ResizeObserver(measureHeight);
		heightObserver.observe(doc.documentElement);
		if (doc.body) heightObserver.observe(doc.body);
		// Width only: the height writes above resize `.paper` too.
		widthObserver = new ResizeObserver(() => {
			if (paper.clientWidth !== fittedWidth) fit();
		});
		widthObserver.observe(paper);
	}

	$effect(() => disconnect);
</script>

<div class="paper" class:loaded bind:this={paper}>
	<iframe
		bind:this={frame}
		src={renderUrl(messageId, images)}
		sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"
		referrerpolicy="no-referrer"
		loading="lazy"
		{title}
		{onload}
	></iframe>
</div>

<style>
	.paper {
		overflow: hidden;
		border-radius: 8px;
		background: #ffffff;
	}

	iframe {
		display: block;
		width: 100%;
		height: 48px;
		border: 0;
		transform-origin: 0 0;
		opacity: 0;
		transition: opacity 160ms var(--ease);
	}

	/* Cancel the thread (6px) and card (6px) gutters so the mail spans the screen. */
	@media (max-width: 720px) {
		.paper {
			margin: 0 -12px;
			border-radius: 0;
		}
	}

	.loaded iframe {
		opacity: 1;
	}
</style>
