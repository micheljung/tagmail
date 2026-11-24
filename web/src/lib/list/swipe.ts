import type { OpIntent } from '../ops/effects';

export type SwipeDirection = 'left' | 'right';

/** Swipes only move threads; right goes to Trash and never deletes for good. */
export const SWIPE_OPS = {
	left: 'archive',
	right: 'trash'
} as const satisfies Record<SwipeDirection, OpIntent['kind']>;

export interface SwipeAllowed {
	left: boolean;
	right: boolean;
}

export interface SwipePoint {
	pointerId: number;
	pointerType: string;
	x: number;
	y: number;
	/** Milliseconds, as in `event.timeStamp`. */
	t: number;
}

export type SwipeOutcome =
	{ kind: 'commit'; direction: SwipeDirection } | { kind: 'snapBack' } | { kind: 'none' };

/** Movement before the gesture picks an axis. */
export const AXIS_LOCK_PX = 10;
/** Share of the row width that commits at any speed. */
export const COMMIT_RATIO = 0.35;
/** A flick commits from this far when it is fast enough. */
export const FLICK_PX = 64;
export const FLICK_PX_PER_MS = 0.5;
/** Recent samples the release velocity is measured over. */
const VELOCITY_WINDOW_MS = 100;
/** The browser's click after a touch release lands within this. */
const CLICK_AFTER_RELEASE_MS = 400;

type Phase = 'idle' | 'pending' | 'horizontal' | 'vertical';

/**
 * A touch swipe on one row: locks to an axis after a few pixels, follows
 * the finger horizontally, and on release commits or snaps back. Mouse and
 * pen never swipe.
 */
export class SwipeTracker {
	#phase: Phase = 'idle';
	#pointerId = -1;
	#startX = 0;
	#startY = 0;
	#samples: { x: number; t: number }[] = [];
	#dx = 0;

	constructor(
		private readonly width: () => number,
		private readonly allowed: () => SwipeAllowed
	) {}

	/** The row's horizontal offset; 0 unless swiping. */
	get dx(): number {
		return this.#dx;
	}

	get swiping(): boolean {
		return this.#phase === 'horizontal';
	}

	down(p: SwipePoint): boolean {
		if (p.pointerType !== 'touch' || this.#phase !== 'idle') return false;
		const allowed = this.allowed();
		if (!allowed.left && !allowed.right) return false;
		this.#phase = 'pending';
		this.#pointerId = p.pointerId;
		this.#startX = p.x;
		this.#startY = p.y;
		this.#samples = [{ x: p.x, t: p.t }];
		this.#dx = 0;
		return true;
	}

	/** Returns whether the move now belongs to the swipe. */
	move(p: SwipePoint): boolean {
		if (p.pointerId !== this.#pointerId) return false;
		const x = p.x - this.#startX;
		const y = p.y - this.#startY;
		if (this.#phase === 'pending') {
			if (Math.max(Math.abs(x), Math.abs(y)) < AXIS_LOCK_PX) return false;
			this.#phase = Math.abs(x) > Math.abs(y) ? 'horizontal' : 'vertical';
		}
		if (this.#phase !== 'horizontal') return false;
		this.#samples.push({ x: p.x, t: p.t });
		while (this.#samples.length > 2 && p.t - this.#samples[0].t > VELOCITY_WINDOW_MS) {
			this.#samples.shift();
		}
		const allowed = this.allowed();
		this.#dx = (x < 0 && !allowed.left) || (x > 0 && !allowed.right) ? 0 : x;
		return true;
	}

	up(p: SwipePoint): SwipeOutcome {
		if (p.pointerId !== this.#pointerId) return { kind: 'none' };
		const wasSwiping = this.#phase === 'horizontal';
		if (wasSwiping) this.move(p);
		const dx = this.#dx;
		const first = this.#samples[0];
		const dt = p.t - first.t;
		const velocity = dt > 0 ? Math.abs(p.x - first.x) / dt : 0;
		this.#reset();
		if (!wasSwiping) return { kind: 'none' };
		const distance = Math.abs(dx);
		const commits =
			distance > 0 &&
			(distance >= this.width() * COMMIT_RATIO ||
				(distance >= FLICK_PX && velocity >= FLICK_PX_PER_MS));
		if (!commits) return { kind: 'snapBack' };
		return { kind: 'commit', direction: dx < 0 ? 'left' : 'right' };
	}

	cancel(): SwipeOutcome {
		const wasSwiping = this.#phase === 'horizontal';
		this.#reset();
		return wasSwiping ? { kind: 'snapBack' } : { kind: 'none' };
	}

	#reset(): void {
		this.#phase = 'idle';
		this.#pointerId = -1;
		this.#samples = [];
		this.#dx = 0;
	}
}

export interface SwipeParams {
	allowed: () => SwipeAllowed;
	/** The offset changed; 0 with `settling` means a snap back. */
	onmove: (dx: number, settling: boolean) => void;
	oncommit: (direction: SwipeDirection) => void;
}

/** A click that follows a drag must not open the thread. */
function swallowNextClick(node: HTMLElement): void {
	const swallow = (e: Event) => {
		e.preventDefault();
		e.stopPropagation();
	};
	const stop = () => {
		clearTimeout(timer);
		node.removeEventListener('click', swallow, { capture: true });
		window.removeEventListener('pointerdown', stop, { capture: true });
	};
	const timer = setTimeout(stop, CLICK_AFTER_RELEASE_MS);
	node.addEventListener('click', swallow, { capture: true, once: true });
	window.addEventListener('pointerdown', stop, { capture: true, once: true });
}

function capture(node: HTMLElement, pointerId: number): void {
	try {
		node.setPointerCapture(pointerId);
	} catch {
		// A pointer the browser no longer tracks keeps its own target.
	}
}

function point(e: PointerEvent): SwipePoint {
	return {
		pointerId: e.pointerId,
		pointerType: e.pointerType,
		x: e.clientX,
		y: e.clientY,
		t: e.timeStamp
	};
}

/** Svelte action binding a {@link SwipeTracker} to a row's pointer events. */
export function swipe(node: HTMLElement, params: SwipeParams) {
	let current = params;
	const tracker = new SwipeTracker(
		() => node.getBoundingClientRect().width,
		() => current.allowed()
	);

	function finish(outcome: SwipeOutcome) {
		if (outcome.kind === 'none') return;
		swallowNextClick(node);
		current.onmove(0, outcome.kind === 'snapBack');
		if (outcome.kind === 'commit') current.oncommit(outcome.direction);
	}

	const onDown = (e: PointerEvent) => void tracker.down(point(e));
	const onMove = (e: PointerEvent) => {
		const wasSwiping = tracker.swiping;
		if (!tracker.move(point(e))) return;
		if (!wasSwiping) capture(node, e.pointerId);
		current.onmove(tracker.dx, false);
	};
	const onUp = (e: PointerEvent) => finish(tracker.up(point(e)));
	const onCancel = () => finish(tracker.cancel());

	node.addEventListener('pointerdown', onDown);
	node.addEventListener('pointermove', onMove);
	node.addEventListener('pointerup', onUp);
	node.addEventListener('pointercancel', onCancel);
	return {
		update(next: SwipeParams) {
			current = next;
		},
		destroy() {
			node.removeEventListener('pointerdown', onDown);
			node.removeEventListener('pointermove', onMove);
			node.removeEventListener('pointerup', onUp);
			node.removeEventListener('pointercancel', onCancel);
		}
	};
}
