# Session Archive / Delete — Frontend UI

Backend already implemented archive (`POST /sessions/{id}/archive`) and permanent
delete (`DELETE /sessions/{id}`); this adds the UI plus the one read route the UI
needs to reach an archived session.

## Backend constraints that shaped the design

- Archive is **one-way** and drops the session from every active listing
  (`SessionStore::list` skips dirs with the `.archived` marker).
- Delete requires **archived-first** (`409` otherwise) — the deliberate two-step
  confirmation gate.
- There was **no** list-archived route → an archived session was invisible to the
  client and delete was unreachable. Added `GET /workspaces/{id}/sessions/archived`
  (workspace-scoped, like the active listing — a panel only ever shows its own
  workspace's sessions, active or retired).

## Flow (implemented)

Two-step: an **archive** row action retires the session from the main list; a
collapsible **已归档** section at the bottom of the sidebar lists archived
sessions, each with a **permanent delete** guarded by a confirm dialog.

## Changes

Backend:

- `SessionStore::list_archived` + shared `list_ids(want_archived)` helper — `src/session/mod.rs`
- `Registry::list_archived_metas` (no workspace-map seeding) — `src/gateway/registry.rs`
- `GET /workspaces/{id}/sessions/archived` → `{ sessions: [SessionMeta] }`, filtered
  by the shared `in_workspace` helper also used by the active listing —
  `src/gateway/server.rs`
- Unit test: `list` and `list_archived` partition the store; archive moves across,
  delete removes from the archived side — `src/session/mod.rs`
- HTTP test: the archived route filters to the workspace, like the active listing —
  `src/gateway/server.rs`

Frontend:

- Endpoints `workspaceArchivedSessions` + `archive` — `client-core/endpoints.ts`
- Client `listArchivedSessions(workspaceId)` / `archiveSession` / `deleteSession` — `types.ts` + `gateway-transport.ts`
- `ConfirmDialog.svelte` (new, `--z-modal` backdrop, Escape cancels, Enter confirms
  only when non-danger, failed confirms surface inside the dialog)
- `Button.svelte` gained a `danger` variant (`--error` / `--error-hover` / `--error-fg`
  tokens added to `tokens.css`, both themes)
- Sidebar: hover archive icon per row + 已归档 section (workspace-scoped, reloads on
  expand / workspace switch / archive) with delete → confirm —
  `routes/workspaces/[wsId]/+layout.svelte`

## Verified

- `cargo test` (session unit tests, incl. new partition test) green.
- `npm run check` — 0 errors / 0 warnings.
- E2E via live gateway: pre-archive delete → 409; archive → 204 (leaves `list`,
  appears in `archived` with meta); post-archive delete → 204 (leaves
  `archived`); get deleted → 404.

## Out of scope (deliberate)

- No restore/unarchive (backend omits it by design).
- No bulk delete.
