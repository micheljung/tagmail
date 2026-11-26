import { describe, expect, it } from 'vitest';
import { ApiError } from '../transport';
import {
	classifyPushError,
	detectBrowser,
	selfChecks,
	troubleshootingSteps,
	type CheckInput
} from './diagnose';

const CHROME_UA = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/147.0 Safari/537.36';
const FIREFOX_UA = 'Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0';
const SAFARI_UA =
	'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15';

describe('classifyPushError', () => {
	it('maps the DOMExceptions pushManager.subscribe raises', () => {
		expect(classifyPushError(new DOMException('x', 'AbortError'))).toBe('push-service');
		expect(classifyPushError(new DOMException('x', 'NotAllowedError'))).toBe('permission');
		expect(classifyPushError(new DOMException('x', 'InvalidStateError'))).toBe('stale');
	});

	it('blames the server for API errors', () => {
		expect(classifyPushError(new ApiError(500, { error: 'internal', message: 'boom' }))).toBe(
			'server'
		);
	});

	it('does not guess for anything else', () => {
		expect(classifyPushError(new DOMException('x', 'NotSupportedError'))).toBe('unknown');
		expect(classifyPushError(new Error('x'))).toBe('unknown');
		expect(classifyPushError('x')).toBe('unknown');
	});
});

describe('detectBrowser', () => {
	it('tells the browsers apart', () => {
		expect(detectBrowser(CHROME_UA, true)).toBe('brave');
		expect(detectBrowser(CHROME_UA, false)).toBe('chromium');
		expect(detectBrowser(FIREFOX_UA, false)).toBe('firefox');
		expect(detectBrowser(SAFARI_UA, false)).toBe('safari');
		expect(detectBrowser('curl/8', false)).toBe('other');
	});
});

describe('troubleshootingSteps', () => {
	const text = (steps: { title: string; detail: string }[]) =>
		steps.map((s) => `${s.title} ${s.detail}`).join('\n');

	it('names the Brave setting for Brave only', () => {
		const brave = text(troubleshootingSteps('push-service', 'brave'));
		expect(brave).toContain('brave://settings/privacy');
		expect(text(troubleshootingSteps('push-service', 'firefox'))).not.toContain('brave');
	});

	it('keeps chrome:// advice away from Firefox', () => {
		const firefox = text(troubleshootingSteps('push-service', 'firefox'));
		expect(firefox).not.toContain('chrome://');
		expect(firefox).toContain('dom.push.enabled');
	});

	it('gives Chromium at least three steps, including the gcm check', () => {
		const steps = troubleshootingSteps('push-service', 'chromium');
		expect(steps.length).toBeGreaterThanOrEqual(3);
		expect(text(steps)).toContain('chrome://gcm-internals');
	});

	it('falls back to generic steps for an unlisted browser', () => {
		expect(troubleshootingSteps('push-service', 'safari').length).toBeGreaterThan(0);
	});

	it('points a server failure at the server, not the browser', () => {
		const server = text(troubleshootingSteps('server', 'chromium'));
		expect(server).toContain('--public-origin');
		expect(server).not.toContain('gcm-internals');
	});
});

describe('selfChecks', () => {
	const base: CheckInput = {
		secureContext: true,
		workerRegistered: true,
		permission: 'granted',
		serverReachable: true
	};
	const state = (input: CheckInput, label: string) =>
		selfChecks(input).find((c) => c.label.includes(label))?.state;

	it('passes when everything works, ruling the server out', () => {
		expect(selfChecks(base).every((c) => c.state === 'ok')).toBe(true);
	});

	it('flags what is wrong', () => {
		expect(state({ ...base, secureContext: false }, 'https')).toBe('fail');
		expect(state({ ...base, workerRegistered: false }, 'worker')).toBe('fail');
		expect(state({ ...base, permission: 'denied' }, 'permission')).toBe('fail');
		expect(state({ ...base, serverReachable: false }, 'server')).toBe('fail');
	});

	it('says unknown instead of guessing', () => {
		expect(state({ ...base, permission: 'default' }, 'permission')).toBe('unknown');
		expect(state({ ...base, serverReachable: null }, 'server')).toBe('unknown');
	});
});
