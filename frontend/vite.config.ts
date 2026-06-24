import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// Dev only: proxy /api to the local gateway so the browser sees one origin
// (no CORS) and SPA routes like /sessions never clash with the API. In
// production the gateway serves the SPA from the same origin, so no proxy is
// needed (doc/gateway.md). Override the target with GATEWAY_URL.
const gateway = process.env.GATEWAY_URL ?? 'http://127.0.0.1:7878';

export default defineConfig({
	plugins: [sveltekit()],
	server: {
		// Bind IPv4 loopback explicitly. Vite otherwise binds IPv6-only ([::1]),
		// so a browser hitting 127.0.0.1 gets connection-refused.
		host: '127.0.0.1',
		proxy: {
			'/api': {
				target: gateway,
				changeOrigin: true,
				// SSE needs an unbuffered, persistent connection; ws covers the
				// /api/.../ws upgrade.
				ws: true
			}
		}
	}
});
