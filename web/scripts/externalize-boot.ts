import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { externalizeBoot } from './boot.ts';

const dir = process.argv[2] ?? 'build';
const page = join(dir, 'index.html');
const { html, files } = externalizeBoot(readFileSync(page, 'utf8'));
if (files.length === 0) throw new Error(`${page} has no inline script to move`);
for (const file of files) writeFileSync(join(dir, file.name), file.content);
writeFileSync(page, html);
console.log(`moved ${files.length} inline script(s) out of ${page}`);
