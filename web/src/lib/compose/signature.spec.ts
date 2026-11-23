import { describe, expect, it } from 'vitest';
import { findSignature, insertSignature, signatureBlock, swapSignature } from './signature';

const A = { id: 1, signature: '<b>Ann</b><br>Personal' };
const B = { id: 2, signature: '<div>Berg team</div>' };
const NONE = { id: 3, signature: null };
const REPLY = '<p><br></p><p>On Mon, Ann wrote:</p><blockquote><p>hi</p></blockquote>';

function signed(html: string, identity: typeof A, quoting = false) {
	const out = insertSignature(html, signatureBlock(identity)!, quoting);
	return { html: out, inserted: findSignature(out)!.inner };
}

describe('signatures', () => {
	it('appends to a new message and goes above the quote in a reply', () => {
		expect(signed('', A).html).toBe(
			'<div><br></div><div class="tagmail-signature" data-identity="1">-- <br><b>Ann</b><br>Personal</div>'
		);
		expect(signed(REPLY, A, true).html).toBe(
			'<p><br></p><div class="tagmail-signature" data-identity="1">-- <br><b>Ann</b><br>Personal</div>' +
				'<div><br></div><p>On Mon, Ann wrote:</p><blockquote><p>hi</p></blockquote>'
		);
	});

	it('finds a block with nested divs', () => {
		const { html } = signed('<div>hello</div>', B);
		expect(findSignature(html)?.inner).toBe('-- <br><div>Berg team</div>');
		expect(html.slice(findSignature(html)!.end)).toBe('');
	});

	it('swaps an unchanged signature for the new identity', () => {
		const { html, inserted } = signed(REPLY, A, true);
		const next = swapSignature(html.replace('<p><br></p>', '<p>Thanks!</p>'), inserted, B, true);
		expect(next).toBe(
			'<p>Thanks!</p><div class="tagmail-signature" data-identity="2">-- <br><div>Berg team</div></div>' +
				'<div><br></div><p>On Mon, Ann wrote:</p><blockquote><p>hi</p></blockquote>'
		);
	});

	it('keeps a signature the writer edited', () => {
		const { html, inserted } = signed('', A);
		const edited = html.replace('Personal', 'Personal, on the train');
		expect(swapSignature(edited, inserted, B, false)).toBeNull();
	});

	it('removes the block for an identity without a signature', () => {
		const { html, inserted } = signed('<div>Hi</div>', A);
		expect(swapSignature(html, inserted, NONE, false)).toBe('<div>Hi</div>');
	});

	it('adds a signature when the previous identity had none, but not after the writer deleted it', () => {
		expect(swapSignature(REPLY, null, A, true)).toBe(signed(REPLY, A, true).html);
		const { inserted } = signed('', A);
		expect(swapSignature('<div>Hi</div>', inserted, B, false)).toBeNull();
		expect(swapSignature('<div>Hi</div>', null, NONE, false)).toBeNull();
	});
});
