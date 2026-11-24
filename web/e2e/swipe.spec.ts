import { expect, openInbox, row, test } from './helpers.ts';

const DRAG_STEPS = 12;

test('a mouse drag across a row never swipes it', async ({ page }) => {
	await openInbox(page);
	const target = row(page, 0).locator('.row');
	const box = (await target.boundingBox())!;
	const y = box.y + box.height / 2;
	await page.mouse.move(box.x + box.width - 20, y);
	await page.mouse.down();
	await page.mouse.move(box.x + 20, y, { steps: DRAG_STEPS });
	await expect(target).not.toHaveAttribute('style', /translateX/);
	await expect(row(page, 0).locator('.under')).toHaveCount(0);
	await page.mouse.up();
	await expect(page.locator('[data-toast]')).toHaveCount(0);
	await page.goto('/inbox');
	await expect(page.locator('.total')).toHaveText('100,000 conversations');
});
