import { THREAD_COUNT } from './mock/fixture.ts';
import { expect, list, openInbox, signIn, test } from './helpers.ts';

const FIRST_RENDER_BUDGET_MS = 1000;
const JUMP_BUDGET_MS = 300;
const FRAME_BUDGET_MS = 16.7;
const SCROLL_FRAMES = 240;
/** Bounds the soft-gate run even where rAF is throttled. */
const SCROLL_MAX_MS = 8000;

declare global {
	interface Window {
		__firstRowAt?: number;
	}
}

function percentile(values: number[], p: number): number {
	const sorted = [...values].sort((a, b) => a - b);
	return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))];
}

test('inbox shows its first conversations within a second', async ({ page }) => {
	await signIn(page);
	await page.addInitScript(() => {
		new MutationObserver((_, observer) => {
			if (document.querySelector('[role="option"] [data-loaded]')) {
				window.__firstRowAt = performance.now();
				observer.disconnect();
			}
		}).observe(document, { childList: true, subtree: true });
	});
	await page.goto('/inbox');
	const firstRow = await page.waitForFunction(() => window.__firstRowAt).then((h) => h.jsonValue());
	test.info().annotations.push({ type: 'inbox first row (ms)', description: firstRow!.toFixed(1) });
	console.log(`[perf] inbox first row with data: ${firstRow!.toFixed(1)} ms`);
	expect(firstRow).toBeLessThan(FIRST_RENDER_BUDGET_MS);
	await expect(
		page.getByText(`${THREAD_COUNT.toLocaleString('en-US')} conversations`)
	).toBeVisible();
});

interface JumpPhases {
	target: number;
	/** First scroll event after setting `scrollTop`. */
	scroll: number;
	/** Row element (skeleton) in the DOM. */
	skeleton: number;
	/** First `/api/threads` request started, and how long it took. */
	fetchStart: number;
	fetchMs: number;
	/** Row data in the DOM. */
	data: number;
	/** First animation frame that sees the data: the gated number. */
	painted: number;
}

test('jumping to row 99,000 renders it within 300 ms', async ({ page }) => {
	await openInbox(page);
	const results: JumpPhases[] = [];
	for (const target of [99_000, 50_000, 99_999, 99_000]) {
		const phases = await list(page).evaluate(
			(el, target) =>
				new Promise<JumpPhases>((resolve, reject) => {
					const rowHeight = el.querySelector<HTMLElement>('[role="option"]')!.offsetHeight;
					const marks: Partial<JumpPhases> = { target };
					performance.clearResourceTimings();
					const t0 = performance.now();
					const since = () => Math.round((performance.now() - t0) * 10) / 10;
					el.addEventListener('scroll', () => (marks.scroll ??= since()), { once: true });
					const observer = new MutationObserver(() => {
						if (marks.skeleton === undefined && el.querySelector(`[data-index="${target}"]`)) {
							marks.skeleton = since();
						}
						if (el.querySelector(`[data-index="${target}"] [data-loaded]`)) {
							marks.data = since();
							observer.disconnect();
						}
					});
					observer.observe(el, { childList: true, subtree: true });
					el.scrollTop = target * rowHeight;
					const tick = () => {
						if (marks.data !== undefined) {
							marks.painted = since();
							const fetches = performance
								.getEntriesByType('resource')
								.filter((e) => e.name.includes('/api/threads?'));
							marks.fetchStart = fetches.length ? Math.round(fetches[0].startTime - t0) : -1;
							marks.fetchMs = fetches.length ? Math.round(fetches[0].duration) : -1;
							return resolve(marks as JumpPhases);
						}
						if (performance.now() - t0 > 5000) {
							observer.disconnect();
							return reject(new Error(`row ${target} never loaded`));
						}
						requestAnimationFrame(tick);
					};
					requestAnimationFrame(tick);
				}),
			target
		);
		results.push(phases);
		console.log(`[perf] jump ${JSON.stringify(phases)}`);
	}
	test.info().annotations.push({
		type: 'jump to row (ms)',
		description: results.map((r) => r.painted.toFixed(1)).join(', ')
	});
	for (const r of results) expect(r.painted).toBeLessThan(JUMP_BUDGET_MS);
	await expect(page.locator('[role="option"][data-index="99000"]')).toBeVisible();
});

test('scripted scroll frame times (reported, soft gate)', async ({ page }) => {
	await openInbox(page);
	const frames = await list(page).evaluate(
		(el, { maxFrames, maxMs }) =>
			new Promise<number[]>((resolve) => {
				const deltas: number[] = [];
				const start = performance.now();
				let last = start;
				const tick = (t: number) => {
					deltas.push(t - last);
					last = t;
					el.scrollTop += 25;
					const done = deltas.length >= maxFrames || t - start > maxMs;
					if (done) resolve(deltas.slice(2));
					else requestAnimationFrame(tick);
				};
				requestAnimationFrame(tick);
			}),
		{ maxFrames: SCROLL_FRAMES, maxMs: SCROLL_MAX_MS }
	);
	const p95 = percentile(frames, 0.95);
	const over = frames.filter((f) => f > FRAME_BUDGET_MS * 1.2).length;
	const summary = `p50 ${percentile(frames, 0.5).toFixed(1)} ms, p95 ${p95.toFixed(1)} ms, max ${Math.max(...frames).toFixed(1)} ms, ${over}/${frames.length} frames over 20 ms`;
	console.log(`[perf] scroll 25 px/frame: ${summary}`);
	test.info().annotations.push({ type: 'scroll frame times', description: summary });
	expect(frames.length).toBeGreaterThan(10);
});
