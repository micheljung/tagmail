declare global {
	namespace App {
		interface PageState {
			/** History entries between this one and the one the app was entered on. */
			appIndex?: number;
			/** Set on the entry a phone selection pushed, so Back clears the selection. */
			selection?: boolean;
		}
	}
}

export {};
