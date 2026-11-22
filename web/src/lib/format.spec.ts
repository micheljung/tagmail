import { describe, expect, it } from 'vitest';
import { displayName, fileSize, labelHue, listDate, senderLine } from './format';

describe('format', () => {
	it('names senders', () => {
		expect(displayName({ name: ' Ana Sousa ', email: 'ana@x.test' })).toBe('Ana Sousa');
		expect(displayName({ name: null, email: 'office@x.test' })).toBe('office');
		expect(displayName(null)).toBe('Unknown sender');
		expect(
			senderLine([
				{ name: 'Ana Sousa', email: 'a@x' },
				{ name: null, email: 'bo@x' }
			])
		).toBe('Ana, bo');
	});

	it('formats list dates like Gmail', () => {
		const now = new Date(2026, 10, 20, 16, 0);
		const at = (d: Date) => d.getTime() / 1000;
		expect(listDate(at(new Date(2026, 10, 20, 9, 5)), now, 'en-US')).toBe('9:05 AM');
		expect(listDate(at(new Date(2026, 2, 4)), now, 'en-US')).toBe('Mar 4');
		expect(listDate(at(new Date(2024, 2, 4)), now, 'en-US')).toBe('3/4/24');
	});

	it('sizes files', () => {
		expect(fileSize(512)).toBe('512 B');
		expect(fileSize(1536)).toBe('1.5 KB');
		expect(fileSize(248_331)).toBe('243 KB');
		expect(fileSize(5 * 1024 * 1024)).toBe('5.0 MB');
	});

	it('gives each label a stable hue', () => {
		expect(labelHue('finance')).toBe(labelHue('finance'));
		expect(labelHue('finance')).not.toBe(labelHue('family'));
		expect(labelHue('x')).toBeGreaterThanOrEqual(0);
		expect(labelHue('x')).toBeLessThan(360);
	});
});
