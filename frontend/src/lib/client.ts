// The app's single SessionClient. The gateway base URL + token come from
// public env (PUBLIC_GATEWAY_URL / PUBLIC_GATEWAY_TOKEN). The default is the
// empty string — a same-origin relative base, which is correct both for
// production (the gateway serves the SPA) and for dev (vite proxies /api to
// the gateway, avoiding CORS; see vite.config.ts). Set PUBLIC_GATEWAY_URL to
// point at a remote gateway (Desktop/multi-server lands in Phase 9).

import { env } from '$env/dynamic/public';
import { GatewayTransport, type SessionClient } from './client-core';

const baseUrl = env.PUBLIC_GATEWAY_URL ?? '';
const token = env.PUBLIC_GATEWAY_TOKEN || undefined;

export const client: SessionClient = new GatewayTransport({ baseUrl, token });
