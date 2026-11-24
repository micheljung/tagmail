declare global {
	namespace App {
		interface PageState {
			/** History entries between this one and the one the app was entered on. */
			appIndex?: number;
		}
	}
}

export {};
