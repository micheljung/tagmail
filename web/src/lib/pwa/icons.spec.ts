import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const STATIC = join(import.meta.dirname, '../../../static');
const PNG_SIGNATURE = '89504e470d0a1a0a';
const RGB = 2;
const RGBA = 6;

interface Icon {
	src: string;
	sizes: string;
	type: string;
	purpose?: string;
}

interface Manifest {
	id: string;
	start_url: string;
	scope: string;
	display: string;
	icons: Icon[];
}

/** Width, height and colour type from a PNG's IHDR chunk. */
function header(path: string) {
	const png = readFileSync(join(STATIC, path));
	expect(png.subarray(0, 8).toString('hex')).toBe(PNG_SIGNATURE);
	expect(png.subarray(12, 16).toString('ascii')).toBe('IHDR');
	return { width: png.readUInt32BE(16), height: png.readUInt32BE(20), colourType: png[25] };
}

const manifest = JSON.parse(readFileSync(join(STATIC, 'manifest.webmanifest'), 'utf8')) as Manifest;

describe('web app manifest', () => {
	it('opens standalone on the inbox', () => {
		expect(manifest).toMatchObject({
			id: '/',
			start_url: '/inbox',
			scope: '/',
			display: 'standalone'
		});
	});

	it.each(manifest.icons.map((icon) => [icon.src, icon] as const))(
		'%s matches its declared size',
		(src, icon) => {
			const [width, height] = icon.sizes.split('x').map(Number);
			expect(icon.type).toBe('image/png');
			expect(header(src)).toMatchObject({ width, height });
		}
	);

	it('has a 512 px maskable icon', () => {
		expect(manifest.icons.some((i) => i.purpose === 'maskable' && i.sizes === '512x512')).toBe(
			true
		);
	});
});

describe('extra icons', () => {
	it('has an opaque 180 px Apple touch icon', () => {
		expect(header('/icons/apple-touch-icon.png')).toEqual({
			width: 180,
			height: 180,
			colourType: RGB
		});
	});

	it('has a 96 px badge with an alpha channel', () => {
		expect(header('/icons/badge-96.png')).toEqual({ width: 96, height: 96, colourType: RGBA });
	});
});
