export type ThemeChoice = 'system' | 'light' | 'dark';

const KEY = 'tagmail.theme';

function read(): ThemeChoice {
	try {
		const v = localStorage.getItem(KEY);
		return v === 'light' || v === 'dark' ? v : 'system';
	} catch {
		return 'system';
	}
}

class Theme {
	choice = $state<ThemeChoice>('system');

	init(): void {
		this.apply(read());
	}

	apply(choice: ThemeChoice): void {
		this.choice = choice;
		const root = document.documentElement;
		if (choice === 'system') root.removeAttribute('data-theme');
		else root.dataset.theme = choice;
		try {
			if (choice === 'system') localStorage.removeItem(KEY);
			else localStorage.setItem(KEY, choice);
		} catch {
			// Storage can be unavailable (private mode); the choice still applies now.
		}
	}

	cycle(): void {
		const dark = matchMedia('(prefers-color-scheme: dark)').matches;
		const effective = this.choice === 'system' ? (dark ? 'dark' : 'light') : this.choice;
		this.apply(effective === 'dark' ? 'light' : 'dark');
	}
}

export const theme = new Theme();
