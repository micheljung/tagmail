<script lang="ts">
	import { renderUrl } from '../transport';

	let { messageId, images, title }: { messageId: number; images: boolean; title: string } =
		$props();

	const MIN_HEIGHT = 48;

	let frame: HTMLIFrameElement;
	let height = $state(MIN_HEIGHT);
	let loaded = $state(false);
	let observer: ResizeObserver | null = null;

	function measure() {
		const doc = frame.contentDocument;
		if (!doc?.documentElement) return;
		const h = Math.max(doc.documentElement.scrollHeight, doc.body?.scrollHeight ?? 0);
		height = Math.max(MIN_HEIGHT, Math.ceil(h));
	}

	function onload() {
		observer?.disconnect();
		loaded = true;
		const doc = frame.contentDocument;
		if (!doc?.documentElement) return;
		measure();
		observer = new ResizeObserver(measure);
		observer.observe(doc.documentElement);
		if (doc.body) observer.observe(doc.body);
	}

	$effect(() => () => observer?.disconnect());
</script>

<div class="paper" class:loaded>
	<iframe
		bind:this={frame}
		src={renderUrl(messageId, images)}
		sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"
		referrerpolicy="no-referrer"
		loading="lazy"
		{title}
		style:height="{height}px"
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
		border: 0;
		opacity: 0;
		transition: opacity 160ms var(--ease);
	}

	.loaded iframe {
		opacity: 1;
	}
</style>
