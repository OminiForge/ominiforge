import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	preprocess: vitePreprocess(),
	kit: {
		// SPA: prerender the shell, fall back to index.html for client-routed paths
		// (doc/frontend.md §1, §5). The gateway serves these static assets.
		adapter: adapter({
			fallback: 'index.html'
		})
	}
};

export default config;
