import { describe, expect, it } from 'vitest';
import { SwipeTracker, type SwipeAllowed, type SwipePoint } from './swipe';

const WIDTH = 400;
const BOTH: SwipeAllowed = { left: true, right: true };

function tracker(allowed: SwipeAllowed = BOTH) {
	return new SwipeTracker(
		() => WIDTH,
		() => allowed
	);
}

function at(x: number, y: number, t: number, extra: Partial<SwipePoint> = {}): SwipePoint {
	return { pointerId: 1, pointerType: 'touch', x, y, t, ...extra };
}

/** Drags from (200, 50) by `dx`/`dy` in even steps over `ms`, then releases. */
function drag(s: SwipeTracker, dx: number, dy: number, ms: number, steps = 10) {
	s.down(at(200, 50, 0));
	for (let i = 1; i <= steps; i++) {
		s.move(at(200 + (dx * i) / steps, 50 + (dy * i) / steps, (ms * i) / steps));
	}
	return s.up(at(200 + dx, 50 + dy, ms));
}

describe('SwipeTracker', () => {
	it.each(['mouse', 'pen'])('ignores %s pointers', (pointerType) => {
		const s = tracker();
		expect(s.down(at(200, 50, 0, { pointerType }))).toBe(false);
		expect(s.move(at(40, 50, 50, { pointerType }))).toBe(false);
		expect(s.up(at(40, 50, 60, { pointerType }))).toEqual({ kind: 'none' });
		expect(s.dx).toBe(0);
	});

	it('waits for the axis lock before moving', () => {
		const s = tracker();
		s.down(at(200, 50, 0));
		expect(s.move(at(193, 50, 10))).toBe(false);
		expect(s.dx).toBe(0);
		expect(s.move(at(185, 51, 20))).toBe(true);
		expect(s.dx).toBe(-15);
	});

	it('leaves a vertical drag to scrolling', () => {
		const s = tracker();
		s.down(at(200, 50, 0));
		expect(s.move(at(205, 70, 10))).toBe(false);
		expect(s.move(at(40, 75, 20))).toBe(false);
		expect(s.dx).toBe(0);
		expect(s.up(at(40, 75, 30))).toEqual({ kind: 'none' });
	});

	it('snaps back from 30 % of the width', () => {
		expect(drag(tracker(), -WIDTH * 0.3, 0, 1000)).toEqual({ kind: 'snapBack' });
	});

	it('commits from 36 % of the width at any speed', () => {
		expect(drag(tracker(), -WIDTH * 0.36, 0, 2000)).toEqual({
			kind: 'commit',
			direction: 'left'
		});
		expect(drag(tracker(), WIDTH * 0.36, 0, 2000)).toEqual({
			kind: 'commit',
			direction: 'right'
		});
	});

	it('commits a fast 70 px flick', () => {
		expect(drag(tracker(), 70, 0, 60)).toEqual({ kind: 'commit', direction: 'right' });
	});

	it('snaps back from a slow 70 px drag', () => {
		expect(drag(tracker(), 70, 0, 1000)).toEqual({ kind: 'snapBack' });
	});

	it('snaps back from a fast flick shorter than the flick distance', () => {
		expect(drag(tracker(), -50, 0, 30)).toEqual({ kind: 'snapBack' });
	});

	it('snaps back on pointercancel', () => {
		const s = tracker();
		s.down(at(200, 50, 0));
		s.move(at(20, 50, 50));
		expect(s.cancel()).toEqual({ kind: 'snapBack' });
		expect(s.dx).toBe(0);
		expect(s.swiping).toBe(false);
	});

	it('does not move or commit towards a direction that is not offered', () => {
		const s = tracker({ left: true, right: false });
		s.down(at(200, 50, 0));
		s.move(at(390, 50, 50));
		expect(s.dx).toBe(0);
		expect(s.up(at(390, 50, 60))).toEqual({ kind: 'snapBack' });
		expect(drag(s, -200, 0, 100)).toEqual({ kind: 'commit', direction: 'left' });
	});

	it('does not start when no direction is offered', () => {
		const s = tracker({ left: false, right: false });
		expect(s.down(at(200, 50, 0))).toBe(false);
		expect(s.move(at(20, 50, 50))).toBe(false);
	});

	it('follows only the pointer that started', () => {
		const s = tracker();
		s.down(at(200, 50, 0));
		expect(s.move(at(20, 50, 50, { pointerId: 2 }))).toBe(false);
		expect(s.up(at(20, 50, 60, { pointerId: 2 }))).toEqual({ kind: 'none' });
		expect(drag(s, 0, 0, 0)).toEqual({ kind: 'none' });
	});
});
