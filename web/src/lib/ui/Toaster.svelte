<script lang="ts">
	import { toasts } from '../stores';
	import Icon from './Icon.svelte';
</script>

<div class="toaster" role="status" aria-live="polite">
	{#each toasts.items as toast (toast.id)}
		<div class="toast {toast.tone}" data-toast>
			{#if toast.tone === 'error'}<Icon
					name="alert"
					size={18}
				/>{:else if toast.tone === 'progress'}<span class="spin" aria-hidden="true"></span>{/if}
			<span class="text">{toast.text}</span>
			{#each toast.actions as action (action.label)}
				<button
					class="action"
					onclick={() => {
						toasts.dismiss(toast.id);
						action.run();
					}}
				>
					{action.label}
				</button>
			{/each}
			<button class="close" aria-label="Dismiss" onclick={() => toasts.dismiss(toast.id)}>
				<Icon name="close" size={16} />
			</button>
		</div>
	{/each}
</div>

<style>
	.toaster {
		position: fixed;
		left: 24px;
		bottom: 24px;
		z-index: 70;
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 8px;
		pointer-events: none;
	}

	.toast {
		--toast-bg: #1d2128;
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 46px;
		max-width: min(560px, calc(100vw - 32px));
		padding: 4px 6px 4px 16px;
		border-radius: 12px;
		background: var(--toast-bg);
		color: #eef1f5;
		box-shadow: var(--shadow-lg);
		pointer-events: auto;
		animation: up 180ms var(--ease);
	}

	.toast.error {
		--toast-bg: #3b1714;
		color: #ffdcd7;
	}

	@keyframes up {
		from {
			opacity: 0;
			transform: translateY(10px);
		}
	}

	.text {
		flex: 1;
		padding: 6px 0;
	}

	.action {
		height: 34px;
		padding: 0 12px;
		border: 0;
		border-radius: 8px;
		background: none;
		color: #9db7ff;
		font-weight: 650;
		cursor: pointer;
	}

	.error .action {
		color: #ffb4a9;
	}

	.action:hover,
	.close:hover {
		background: rgb(255 255 255 / 0.1);
	}

	.close {
		display: grid;
		place-items: center;
		width: 32px;
		height: 32px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: rgb(255 255 255 / 0.6);
		cursor: pointer;
	}

	.spin {
		width: 14px;
		height: 14px;
		border: 2px solid rgb(255 255 255 / 0.3);
		border-top-color: #fff;
		border-radius: 50%;
		animation: spin 700ms linear infinite;
	}

	@keyframes spin {
		to {
			transform: rotate(1turn);
		}
	}

	@media (max-width: 720px) {
		.toaster {
			left: 12px;
			right: 12px;
			bottom: 12px;
		}

		.toast {
			max-width: none;
			width: 100%;
		}
	}
</style>
