import { describe, expect, it, vi } from 'vitest';
import { claimTimeZone, timeZones } from './zone';

describe('the owner time zone', () => {
	it('is offered once, without replacing one the owner has', async () => {
		const updateReminderSettings = vi.fn(() => Promise.resolve({ timeZone: 'Europe/Zurich' }));
		await claimTimeZone({ updateReminderSettings }, 'Europe/Zurich');
		expect(updateReminderSettings).toHaveBeenCalledWith({
			timeZone: 'Europe/Zurich',
			ifUnset: true
		});
	});

	it('is not offered when unknown, and a failure is quiet', async () => {
		const updateReminderSettings = vi.fn(() => Promise.reject(new Error('offline')));
		await claimTimeZone({ updateReminderSettings }, '');
		expect(updateReminderSettings).not.toHaveBeenCalled();
		await expect(
			claimTimeZone({ updateReminderSettings }, 'Europe/Zurich')
		).resolves.toBeUndefined();
	});

	it('lists a stored zone the browser does not know first', () => {
		expect(timeZones('Etc/Unknown')[0]).toBe('Etc/Unknown');
		expect(timeZones(null)).not.toContain(null);
	});
});
