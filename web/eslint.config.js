import prettier from 'eslint-config-prettier';
import path from 'node:path';
import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import { defineConfig, includeIgnoreFile } from 'eslint/config';
import globals from 'globals';
import ts from 'typescript-eslint';

const gitignorePath = path.resolve(import.meta.dirname, '.gitignore');

export default defineConfig(
	includeIgnoreFile(gitignorePath),
	js.configs.recommended,
	ts.configs.recommendedTypeChecked,
	svelte.configs.recommended,
	prettier,
	svelte.configs.prettier,
	{
		languageOptions: {
			parserOptions: { projectService: true },
			globals: { ...globals.browser, ...globals.node }
		},
		rules: {
			'no-undef': 'off',
			// The SPA is always served at the origin root (web and app://localhost), so
			// links need no base-path resolution.
			'svelte/no-navigation-without-resolve': 'off'
		}
	},
	{
		files: ['**/*.svelte', '**/*.svelte.ts', '**/*.svelte.js'],
		languageOptions: {
			parserOptions: {
				projectService: true,
				extraFileExtensions: ['.svelte'],
				parser: ts.parser
			}
		}
	},
	{
		files: ['**/*.svelte'],
		// typescript-eslint cannot see the prop types of imported components, so
		// callbacks passed to them would all read as `any`; svelte-check covers these.
		rules: {
			'@typescript-eslint/no-unsafe-argument': 'off',
			'@typescript-eslint/no-unsafe-assignment': 'off',
			'@typescript-eslint/no-unsafe-call': 'off',
			'@typescript-eslint/no-unsafe-member-access': 'off',
			'@typescript-eslint/no-unsafe-return': 'off'
		}
	},
	{
		// Id indexes, row caches and op bookkeeping are deliberately non-reactive;
		// a version counter or a replaced $state.raw value tells the UI they changed.
		files: [
			'src/lib/list/**',
			'src/lib/stores/**',
			'src/lib/ops/**',
			'src/lib/compose/**',
			'src/lib/ui/LabelPicker.svelte'
		],
		rules: { 'svelte/prefer-svelte-reactivity': 'off' }
	}
);
