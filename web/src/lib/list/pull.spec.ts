import { describe, expect, it } from 'vitest';
import { PULL_MAX_PX, PULL_THRESHOLD_PX, PullTracker, type PullEvent } from './pull';

/** A synthetic touch event with one finger at (x, y), or `fingers` at that spot. */
function touch(x: number, y: number, cancelable = true, fingers = 1): PullEvent {
	return {
		touches: Array.from({ length: fingers }, () => ({ clientX: x, clientY: y })),
		cancelable
	};
}

function tracker(opts: { enabled?: boolean; scrollTop?: number } = {}) {
	const state = { enabled: opts.enabled ?? true, scrollTop: opts.scrollTop ?? 0 };
	const t = new PullTracker(
		() => state.enabled,
		() => state.scrollTop
	);
	return { t, state };
}

/** Pulls from (100, 100) down by `dy` in even steps; returns the preventDefault answers. */
function pull(t: PullTracker, dy: number, steps = 10, dx = 0): boolean[] {
	t.start(touch(100, 100));
	const out: boolean[] = [];
	for (let i = 1; i <= steps; i++)
		out.push(t.move(touch(100 + (dx * i) / steps, 100 + (dy * i) / steps)));
	return out;
}

describe('PullTracker', () => {
	it('engages on the first downward move and cancels the native pan from then on', () => {
		const { t } = tracker();
		const prevented = pull(t, 140);
		expect(prevented.every(Boolean)).toBe(true);
		expect(t.pulling).toBe(true);
		expect(t.offset).toBe(70);
		expect(t.end()).toBe(true);
		expect(t.offset).toBe(0);
		expect(t.pulling).toBe(false);
	});

	it('applies 0.5 resistance and caps the offset at 96px', () => {
		const { t } = tracker();
		t.start(touch(100, 100));
		t.move(touch(100, 140));
		expect(t.offset).toBe(20);
		t.move(touch(100, 400));
		expect(t.offset).toBe(PULL_MAX_PX);
	});

	it('refreshes only at or past the 64px resisted threshold (128px of travel)', () => {
		const below = tracker().t;
		pull(below, 2 * PULL_THRESHOLD_PX - 2);
		expect(below.offset).toBe(PULL_THRESHOLD_PX - 1);
		expect(below.end()).toBe(false);

		const at = tracker().t;
		pull(at, 2 * PULL_THRESHOLD_PX);
		expect(at.offset).toBe(PULL_THRESHOLD_PX);
		expect(at.end()).toBe(true);
	});

	it('does not arm when disabled, scrolled, or with more than one finger', () => {
		for (const { t, fingers } of [
			{ t: tracker({ enabled: false }).t, fingers: 1 },
			{ t: tracker({ scrollTop: 1 }).t, fingers: 1 },
			{ t: tracker().t, fingers: 2 }
		]) {
			t.start(touch(100, 100, true, fingers));
			expect(t.move(touch(100, 110, true, fingers))).toBe(false);
			expect(t.move(touch(100, 300, true, fingers))).toBe(false);
			expect(t.offset).toBe(0);
			expect(t.end()).toBe(false);
		}
	});

	it('disengages for the whole gesture when the first move goes up or sideways', () => {
		const up = tracker().t;
		up.start(touch(100, 100));
		expect(up.move(touch(100, 98))).toBe(false);
		expect(up.move(touch(100, 300))).toBe(false);
		expect(up.offset).toBe(0);
		expect(up.end()).toBe(false);

		const sideways = tracker().t;
		sideways.start(touch(100, 100));
		expect(sideways.move(touch(106, 105))).toBe(false);
		expect(sideways.move(touch(106, 300))).toBe(false);
		expect(sideways.end()).toBe(false);
	});

	it('engages on a diagonal first move that is at least as vertical as it is wide', () => {
		const { t } = tracker();
		t.start(touch(100, 100));
		expect(t.move(touch(104, 104))).toBe(true);
		expect(t.pulling).toBe(true);
	});

	it('disengages when the first move is not cancelable (the native pan already started)', () => {
		const { t } = tracker();
		t.start(touch(100, 100));
		expect(t.move(touch(100, 110, false))).toBe(false);
		expect(t.move(touch(100, 300))).toBe(false);
		expect(t.end()).toBe(false);
	});

	it('waits for a move with actual movement before deciding', () => {
		const { t } = tracker();
		t.start(touch(100, 100));
		expect(t.move(touch(100, 100))).toBe(false);
		expect(t.move(touch(100, 102))).toBe(true);
	});

	it('snaps back once an engaged pull goes above its start or turns sideways', () => {
		const up = tracker().t;
		pull(up, 140);
		expect(up.move(touch(100, 99))).toBe(false);
		expect(up.offset).toBe(0);
		expect(up.move(touch(100, 300))).toBe(false);
		expect(up.end()).toBe(false);

		const sideways = tracker().t;
		pull(sideways, 140);
		expect(sideways.move(touch(300, 240))).toBe(false);
		expect(sideways.offset).toBe(0);
		expect(sideways.end()).toBe(false);
	});

	it('re-checks enabled on every move: a hold that ticks a row ends the pull', () => {
		const { t, state } = tracker();
		pull(t, 140);
		state.enabled = false;
		expect(t.move(touch(100, 250))).toBe(false);
		expect(t.offset).toBe(0);
		state.enabled = true;
		expect(t.move(touch(100, 260))).toBe(false);
		expect(t.end()).toBe(false);
	});

	it('re-checks enabled on release', () => {
		const { t, state } = tracker();
		pull(t, 140);
		state.enabled = false;
		expect(t.end()).toBe(false);
	});

	it('drops the pull when a second finger lands or the touch is cancelled', () => {
		const two = tracker().t;
		pull(two, 140);
		two.start(touch(100, 240, true, 2));
		expect(two.offset).toBe(0);
		expect(two.end(1)).toBe(false);
		expect(two.end(0)).toBe(false);

		const cancelled = tracker().t;
		pull(cancelled, 140);
		cancelled.cancel();
		expect(cancelled.offset).toBe(0);
		expect(cancelled.end()).toBe(false);
	});

	it('starts fresh on the next gesture after a disengaged one', () => {
		const { t } = tracker();
		t.start(touch(100, 100));
		t.move(touch(100, 90));
		t.end();
		expect(pull(t, 140).every(Boolean)).toBe(true);
		expect(t.end()).toBe(true);
	});
});
