/**
 * Scale that fits content of `natural` px width into `avail` px: 1 unless it
 * overflows by more than 1px, else the ratio to 3 decimals, rounded down so
 * the scaled content never ends past `avail`.
 */
export function fitScale(natural: number, avail: number): number {
	if (!(avail > 0) || natural <= avail + 1) return 1;
	return Math.floor((avail / natural) * 1000 + 1e-9) / 1000;
}
