import { ApiError } from '../transport';

export type PushFailure = 'push-service' | 'permission' | 'stale' | 'server' | 'unknown';
export type PushBrowser = 'brave' | 'chromium' | 'firefox' | 'safari' | 'other';

export interface Step {
	title: string;
	detail: string;
}

export interface Check {
	label: string;
	state: 'ok' | 'fail' | 'unknown';
	hint: string;
}

/** Sorts a failed `enablePush` by where it broke, so the page can say what to do. */
export function classifyPushError(e: unknown): PushFailure {
	if (e instanceof ApiError) return 'server';
	if (e instanceof DOMException) {
		if (e.name === 'AbortError') return 'push-service';
		if (e.name === 'NotAllowedError') return 'permission';
		if (e.name === 'InvalidStateError') return 'stale';
	}
	return 'unknown';
}

export function detectBrowser(userAgent: string, brave: boolean): PushBrowser {
	if (brave) return 'brave';
	if (/Firefox\//.test(userAgent)) return 'firefox';
	if (/Chrome\/|Chromium\/|Edg\//.test(userAgent)) return 'chromium';
	if (/Safari\//.test(userAgent)) return 'safari';
	return 'other';
}

export const CAUSE: Record<PushFailure, string> = {
	'push-service':
		'Your browser could not reach its push service, so it could not register this device. Tagmail’s server was not involved.',
	permission: 'Notifications are blocked for this site in your browser.',
	stale: 'Your browser holds an old notification registration for this site.',
	server: 'Tagmail’s server refused or could not be reached while registering this device.',
	unknown: 'Something went wrong while turning on notifications.'
};

const RELOAD: Step = {
	title: 'Clear this site’s data, reload and try again',
	detail:
		'In the address bar open the site settings and choose “Reset permissions” or “Clear data”.'
};

const STEPS: Record<PushFailure, Partial<Record<PushBrowser, Step[]>> & { other: Step[] }> = {
	'push-service': {
		brave: [
			{
				title: 'Allow Google services for push messaging',
				detail:
					'Open brave://settings/privacy, turn on “Use Google services for push messaging”, restart Brave.'
			},
			RELOAD
		],
		chromium: [
			{
				title: 'Leave private or incognito mode',
				detail: 'Browsers do not offer push notifications in private windows.'
			},
			{
				title: 'Check that the push service is reachable',
				detail:
					'Open chrome://gcm-internals and look for “Connected”. A VPN, firewall or DNS filter may block fcm.googleapis.com.'
			},
			{
				title: 'Use a browser with Google services',
				detail:
					'Chromium builds without Google API keys (many Linux packages, Ungoogled Chromium) cannot register for push. Try Chrome, Edge or Firefox.'
			},
			RELOAD
		],
		firefox: [
			{
				title: 'Leave private browsing',
				detail: 'Firefox does not offer push notifications in private windows.'
			},
			{
				title: 'Check that push is enabled',
				detail: 'Open about:config and make sure dom.push.enabled is true.'
			},
			RELOAD
		],
		other: [
			{
				title: 'Leave private browsing',
				detail: 'Browsers do not offer push notifications in private windows.'
			},
			{
				title: 'Check your network',
				detail: 'A VPN, firewall or DNS filter may block your browser’s push service.'
			},
			{
				title: 'Try another browser',
				detail: 'Chrome, Edge and Firefox support push notifications.'
			},
			RELOAD
		]
	},
	permission: {
		other: [
			{
				title: 'Allow notifications for this site',
				detail:
					'Click the icon left of the address bar, open the site settings and set Notifications to Allow.'
			},
			{ title: 'Reload and try again', detail: 'The change applies after a reload.' }
		]
	},
	stale: { other: [RELOAD] },
	server: {
		other: [
			{
				title: 'Check that you are still signed in',
				detail: 'Reload the page. If it asks for your password, sign in and try again.'
			},
			{
				title: 'Check the server',
				detail:
					'Notifications need the server behind https (--public-origin https://…). Its log shows the failed request.'
			}
		]
	},
	unknown: { other: [RELOAD] }
};

export function troubleshootingSteps(failure: PushFailure, browser: PushBrowser): Step[] {
	const steps = STEPS[failure];
	return steps[browser] ?? steps.other;
}

export interface CheckInput {
	secureContext: boolean;
	workerRegistered: boolean;
	permission: NotificationPermission | undefined;
	/** `null` when the check could not run. */
	serverReachable: boolean | null;
}

/** What the page can verify itself; each line names what it rules out. */
export function selfChecks(input: CheckInput): Check[] {
	return [
		{
			label: 'Page is served over https',
			state: input.secureContext ? 'ok' : 'fail',
			hint: 'Push needs https, or localhost.'
		},
		{
			label: 'App worker is registered',
			state: input.workerRegistered ? 'ok' : 'fail',
			hint: 'Reload the page; if it stays missing, the browser blocks service workers.'
		},
		{
			label: 'Notification permission',
			state:
				input.permission === 'granted' ? 'ok' : input.permission === 'denied' ? 'fail' : 'unknown',
			hint: 'Allow notifications for this site in your browser.'
		},
		{
			label: 'Tagmail’s server answers',
			state: input.serverReachable === null ? 'unknown' : input.serverReachable ? 'ok' : 'fail',
			hint: 'If this passes, the problem is in your browser, not the server.'
		}
	];
}
