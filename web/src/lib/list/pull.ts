/**
 * Pull-to-refresh gesture on the list scroller, fed from non-passive touch
 * events (pointer events are useless here: rows have `touch-action: pan-y`,
 * so the browser takes vertical moves and sends `pointercancel`).
 */

/** Finger travel to indicator offset. */
export const PULL_RESISTANCE = 0.5;
/** Resisted offset a release must reach to refresh (128px of finger travel). */
export const PULL_THRESHOLD_PX = 64;
/** The indicator never moves further than this. */
export const PULL_MAX_PX = 96;

/** The parts of a `TouchEvent` the tracker reads. */
export interface PullEvent {
	readonly touches: ArrayLike<{ readonly clientX: number; readonly clientY: number }>;
	readonly cancelable: boolean;
}

type Phase = 'idle' | 'armed' | 'engaged' | 'off';

export class PullTracker {
	readonly enabled: () => boolean;
	readonly scrollTop: () => number;
	#phase: Phase = 'idle';
	#startX = 0;
	#startY = 0;
	#offset = 0;

	constructor(enabled: () => boolean, scrollTop: () => number) {
		this.enabled = enabled;
		this.scrollTop = scrollTop;
	}

	/** The indicator offset; 0 unless pulling. */
	get offset(): number {
		return this.#offset;
	}

	get pulling(): boolean {
		return this.#phase === 'engaged';
	}

	start(e: PullEvent): void {
		if (this.#phase !== 'idle') {
			// A second finger: the gesture is no longer a pull.
			this.#off();
			return;
		}
		if (!this.enabled() || e.touches.length !== 1 || this.scrollTop() > 0) {
			this.#phase = 'off';
			return;
		}
		this.#phase = 'armed';
		this.#startX = e.touches[0].clientX;
		this.#startY = e.touches[0].clientY;
		this.#offset = 0;
	}

	/** Returns whether the caller must `preventDefault()` this move. */
	move(e: PullEvent): boolean {
		if (this.#phase !== 'armed' && this.#phase !== 'engaged') return false;
		if (!this.enabled() || e.touches.length !== 1) {
			this.#off();
			return false;
		}
		const dx = e.touches[0].clientX - this.#startX;
		const dy = e.touches[0].clientY - this.#startY;
		if (this.#phase === 'armed') {
			if (dx === 0 && dy === 0) return false;
			// Decide on the first move: once the browser starts a native pan,
			// later moves are no longer cancelable.
			if (!e.cancelable || dy <= 0 || Math.abs(dx) > Math.abs(dy)) {
				this.#off();
				return false;
			}
			this.#phase = 'engaged';
		} else if (dy < 0 || Math.abs(dx) > Math.abs(dy)) {
			this.#off();
			return false;
		}
		this.#offset = Math.min(dy * PULL_RESISTANCE, PULL_MAX_PX);
		return true;
	}

	/**
	 * The touch ended; `remaining` is the number of fingers still down.
	 * Returns whether the release asks for a refresh.
	 */
	end(remaining = 0): boolean {
		const refresh =
			this.#phase === 'engaged' && this.#offset >= PULL_THRESHOLD_PX && this.enabled();
		if (remaining > 0) this.#off();
		else this.#reset();
		return refresh;
	}

	cancel(): void {
		this.#reset();
	}

	#off(): void {
		this.#phase = 'off';
		this.#offset = 0;
	}

	#reset(): void {
		this.#phase = 'idle';
		this.#offset = 0;
	}
}
