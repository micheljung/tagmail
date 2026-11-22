import { createHash } from 'node:crypto';

const INLINE_SCRIPT = /<script>([\s\S]*?)<\/script>/g;

export interface ExternalizedBoot {
	html: string;
	files: { name: string; content: string }[];
}

/**
 * Moves SvelteKit's inline bootstrap `<script>` into a file so the page runs
 * under `script-src 'self'`. The file stays a classic script in the same
 * place, because the bootstrap reads `document.currentScript.parentElement`.
 */
export function externalizeBoot(html: string, dir = '_app'): ExternalizedBoot {
	const files: { name: string; content: string }[] = [];
	const out = html.replace(INLINE_SCRIPT, (_match, body: string) => {
		const content = body.trim() + '\n';
		const hash = createHash('sha256').update(content).digest('hex').slice(0, 12);
		const name = `${dir}/boot.${hash}.js`;
		files.push({ name, content });
		return `<script src="/${name}"></script>`;
	});
	return { html: out, files };
}
