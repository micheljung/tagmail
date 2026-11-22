import { THREAD_COUNT } from './mock/fixture.ts';
import { expect, list, openInbox, signIn, test } from './helpers.ts';

const FIRST_RENDER_BUDGET_MS = 1000;
const JUMP_BUDGET_MS = 300;
const FRAME_BUDGET_MS = 16.7;

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

test('jumping to row 99,000 renders it within 300 ms', async ({ page }) => {
	await openInbox(page);
	const results: number[] = [];
	for (const target of [99_000, 50_000, 99_999, 99_000]) {
		const ms = await list(page).evaluate(
			(el, target) =>
				new Promise<number>((resolve, reject) => {
					const rowHeight = el.querySelector<HTMLElement>('[role="option"]')!.offsetHeight;
					const t0 = performance.now();
					el.scrollTop = target * rowHeight;
					const tick = () => {
						const loaded = el.querySelector(`[data-index="${target}"] [data-loaded]`);
						if (loaded) return resolve(performance.now() - t0);
						if (performance.now() - t0 > 5000)
							return reject(new Error(`row ${target} never loaded`));
						requestAnimationFrame(tick);
					};
					requestAnimationFrame(tick);
				}),
			target
		);
		results.push(ms);
		console.log(`[perf] jump to ${target}: ${ms.toFixed(1)} ms`);
	}
	test.info().annotations.push({
		type: 'jump to row (ms)',
		description: results.map((r) => r.toFixed(1)).join(', ')
	});
	for (const ms of results) expect(ms).toBeLessThan(JUMP_BUDGET_MS);
	await expect(page.locator('[role="option"][data-index="99000"]')).toBeVisible();
});

test('scripted scroll frame times (reported, soft gate)', async ({ page }) => {
	await openInbox(page);
	const frames = await list(page).evaluate(
		(el) =>
			new Promise<number[]>((resolve) => {
				const deltas: number[] = [];
				let last = performance.now();
				let n = 0;
				const tick = (t: number) => {
					deltas.push(t - last);
					last = t;
					el.scrollTop += 25;
					if (++n < 240) requestAnimationFrame(tick);
					else resolve(deltas.slice(2));
				};
				requestAnimationFrame(tick);
			})
	);
	const p95 = percentile(frames, 0.95);
	const over = frames.filter((f) => f > FRAME_BUDGET_MS * 1.2).length;
	const summary = `p50 ${percentile(frames, 0.5).toFixed(1)} ms, p95 ${p95.toFixed(1)} ms, max ${Math.max(...frames).toFixed(1)} ms, ${over}/${frames.length} frames over 20 ms`;
	console.log(`[perf] scroll 25 px/frame: ${summary}`);
	test.info().annotations.push({ type: 'scroll frame times', description: summary });
	expect(frames.length).toBeGreaterThan(200);
});
