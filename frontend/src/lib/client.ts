// The app's single SessionClient. The gateway base URL + token come from
// public env (PUBLIC_GATEWAY_URL / PUBLIC_GATEWAY_TOKEN) with a loopback
// default for local dev (gateway.md §7 binds 127.0.0.1:7878). A richer
// runtime server-registry (Desktop, multi-server) lands in Phase 9; for now
// one configured gateway is enough.

import { env } from '$env/dynamic/public';
import { GatewayTransport, type SessionClient } from './client-core';

const baseUrl = env.PUBLIC_GATEWAY_URL ?? 'http://127.0.0.1:7878';
const token = env.PUBLIC_GATEWAY_TOKEN || undefined;

export const client: SessionClient = new GatewayTransport({ baseUrl, token });
