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
		// Dual-stack bind: '::' makes Node listen on IPv6 and (on Linux with the
		// default bindv6only=0) also accept IPv4-mapped connections, so an SSH
		// tunnel forwarding to either 127.0.0.1 or [::1] reaches the dev server.
		// allowedHosts disables vite's host-header check (needed when reached via
		// a tunnel / non-localhost host). Dev only.
		host: '::',
		allowedHosts: true,
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
