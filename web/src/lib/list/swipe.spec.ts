import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
	HOLD_MS,
	SWIPE_OPS,
	SwipeTracker,
	swipe,
	type SwipeAllowed,
	type SwipeParams,
	type SwipePoint
} from './swipe';

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

describe('SWIPE_OPS', () => {
	it('archives left and trashes right, never deleting for good', () => {
		expect(SWIPE_OPS).toEqual({ left: 'archive', right: 'trash' });
	});
});

/** A row stand-in: the action only needs events, a width and pointer capture. */
function row() {
	const node = Object.assign(new EventTarget(), {
		getBoundingClientRect: () => ({ width: WIDTH }),
		setPointerCapture: () => undefined
	});
	return node as unknown as HTMLElement;
}

function pointer(
	node: HTMLElement,
	type: string,
	x: number,
	y: number,
	pointerType = 'touch',
	pointerId = 1
) {
	const e = Object.assign(new Event(type, { cancelable: true }), {
		pointerId,
		pointerType,
		clientX: x,
		clientY: y
	});
	node.dispatchEvent(e);
	return e;
}

/**
 * Dispatches a click through the stubbed window's capture phase, then on the
 * node unless that stopped it; returns whether something swallowed it.
 */
function click(node: EventTarget): boolean {
	const e = new Event('click', { cancelable: true });
	window.dispatchEvent(e);
	if (!e.cancelBubble) node.dispatchEvent(e);
	return e.defaultPrevented;
}

describe('swipe action hold', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		vi.stubGlobal('window', new EventTarget());
	});
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	function setup(allowed: SwipeAllowed = BOTH) {
		const node = row();
		const params: SwipeParams = {
			allowed: () => allowed,
			onmove: vi.fn(),
			oncommit: vi.fn(),
			onhold: vi.fn()
		};
		const action = swipe(node, params);
		return { node, params, action };
	}

	it('fires after HOLD_MS of a still touch', () => {
		const { node, params } = setup();
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS - 1);
		expect(params.onhold).not.toHaveBeenCalled();
		vi.advanceTimersByTime(1);
		expect(params.onhold).toHaveBeenCalledTimes(1);
	});

	it('is cancelled by 10 px of movement', () => {
		const { node, params } = setup();
		pointer(node, 'pointerdown', 200, 50);
		pointer(node, 'pointermove', 200, 59);
		vi.advanceTimersByTime(100);
		pointer(node, 'pointermove', 200, 60);
		vi.advanceTimersByTime(HOLD_MS);
		expect(params.onhold).not.toHaveBeenCalled();
	});

	it('is cancelled by an early release', () => {
		const { node, params } = setup();
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS / 2);
		pointer(node, 'pointerup', 200, 50);
		vi.advanceTimersByTime(HOLD_MS);
		expect(params.onhold).not.toHaveBeenCalled();
		expect(click(node)).toBe(false);
	});

	it.each(['mouse', 'pen'])('never holds a %s pointer', (pointerType) => {
		const { node, params } = setup();
		pointer(node, 'pointerdown', 200, 50, pointerType);
		vi.advanceTimersByTime(HOLD_MS * 2);
		expect(params.onhold).not.toHaveBeenCalled();
	});

	it('is not armed without onhold', () => {
		const node = row();
		const onmove = vi.fn();
		swipe(node, { allowed: () => BOTH, onmove, oncommit: vi.fn() });
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS * 2);
		pointer(node, 'pointerup', 200, 50);
		expect(click(node)).toBe(false);
	});

	it('fires while swiping is off (selection mode)', () => {
		const { node, params } = setup({ left: false, right: false });
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS);
		expect(params.onhold).toHaveBeenCalledTimes(1);
	});

	it('ignores moves after the hold until release', () => {
		const { node, params } = setup();
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS);
		pointer(node, 'pointermove', 100, 52);
		pointer(node, 'pointermove', 20, 55);
		expect(params.onmove).not.toHaveBeenCalled();
		pointer(node, 'pointerup', 20, 55);
		expect(params.oncommit).not.toHaveBeenCalled();
		expect(params.onhold).toHaveBeenCalledTimes(1);
	});

	it('swallows the click that follows a held release', () => {
		const { node } = setup();
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS);
		pointer(node, 'pointerup', 200, 50);
		expect(click(node)).toBe(true);
		expect(click(node)).toBe(false);
	});

	it('swallows a held release click that lands on another row', () => {
		const { node } = setup();
		const other = row();
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS);
		pointer(node, 'pointerup', 200, 50);
		expect(click(other)).toBe(true);
		expect(click(node)).toBe(false);
	});

	it('stops swallowing a held release click after a short while', () => {
		const { node } = setup();
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(HOLD_MS);
		pointer(node, 'pointerup', 200, 50);
		vi.advanceTimersByTime(1000);
		expect(click(row())).toBe(false);
	});

	it('treats an early platform contextmenu as the hold and blocks it', () => {
		const { node, params } = setup();
		pointer(node, 'pointerdown', 200, 50);
		vi.advanceTimersByTime(200);
		const menu = new Event('contextmenu', { cancelable: true });
		node.dispatchEvent(menu);
		expect(menu.defaultPrevented).toBe(true);
		expect(params.onhold).toHaveBeenCalledTimes(1);
		vi.advanceTimersByTime(HOLD_MS);
		expect(params.onhold).toHaveBeenCalledTimes(1);
	});

	it('leaves a mouse contextmenu alone', () => {
		const { node, params } = setup();
		const menu = new Event('contextmenu', { cancelable: true });
		node.dispatchEvent(menu);
		expect(menu.defaultPrevented).toBe(false);
		expect(params.onhold).not.toHaveBeenCalled();
	});

	it('stops a native pan after the hold', () => {
		const { node } = setup();
		const before = new Event('touchmove', { cancelable: true });
		pointer(node, 'pointerdown', 200, 50);
		node.dispatchEvent(before);
		expect(before.defaultPrevented).toBe(false);
		vi.advanceTimersByTime(HOLD_MS);
		const after = new Event('touchmove', { cancelable: true });
		node.dispatchEvent(after);
		expect(after.defaultPrevented).toBe(true);
	});

	it('a swipe past the axis lock cancels the hold', () => {
		const { node, params } = setup();
		pointer(node, 'pointerdown', 200, 50);
		pointer(node, 'pointermove', 170, 50);
		vi.advanceTimersByTime(HOLD_MS);
		expect(params.onhold).not.toHaveBeenCalled();
		expect(params.onmove).toHaveBeenCalledWith(-30, false);
	});
});
