import { describe, expect, it } from 'vitest';
import { externalizeBoot } from './boot';

const PAGE = `<html><body><div class="app-root">
			<script>
				{ const element = document.currentScript.parentElement; }
			</script>
		</div></body></html>`;

describe('externalizeBoot', () => {
	it('replaces the inline bootstrap with a content-hashed classic script', () => {
		const { html, files } = externalizeBoot(PAGE);
		expect(files).toHaveLength(1);
		expect(files[0].name).toMatch(/^_app\/boot\.[0-9a-f]{12}\.js$/);
		expect(files[0].content).toContain('document.currentScript.parentElement');
		expect(html).toContain(`<script src="/${files[0].name}"></script>`);
		expect(html).not.toMatch(/<script>/);
	});

	it('is deterministic', () => {
		expect(externalizeBoot(PAGE)).toEqual(externalizeBoot(PAGE));
	});

	it('leaves external scripts alone', () => {
		const page = '<script src="/x.js"></script><script type="module" src="/y.js"></script>';
		expect(externalizeBoot(page)).toEqual({ html: page, files: [] });
	});
});
