# Upstream boxlite issue draft — jailer fails on NixOS

> Draft for filing against boxlite (0.9.7). Tracks the second of the two
> NixOS jailer blockers documented in `doc/sandbox.md` §5.2. Once filed, link the
> issue URL here and in §5.2.

---

## Title

Jailer fails to start a box on NixOS: `system_ca_paths()` binds a directory *and*
a dangling symlink inside it

## Summary

On NixOS, starting any box with the jailer enabled fails with:

```
bwrap: Can't create file at /etc/ssl/certs/ca-certificates.crt: No such file or directory
Box <id> failed to start (Exit code: 1)
```

Root cause: `jailer/mod.rs::build_path_access` iterates `system_ca_paths()` and
read-only-binds **every path that `exists()`**. On NixOS both of these exist and
are both bound:

- `/etc/ssl/certs` — a directory
- `/etc/ssl/certs/ca-certificates.crt` — a symlink *inside* that directory,
  pointing to `/etc/static/ssl/certs/ca-bundle.crt` →
  `/nix/store/...-nss-cacert-*/etc/ssl/certs/ca-bundle.crt`

`bwrap` binds the parent dir `/etc/ssl/certs` read-only first. The inner path is
then a **dangling symlink** from bwrap's point of view (its target `/etc/static`
/ `/nix/store` is not bound into the sandbox), so when bwrap tries to create the
mount point for the second `--ro-bind`, the `open()` follows the dangling link to
a non-existent target and fails with `Can't create file`.

On standard FHS distros `ca-certificates.crt` is a real file inside
`/etc/ssl/certs`, so binding the dir already exposes it and the second bind is
redundant-but-harmless. NixOS's symlink-farm `/etc` turns "redundant" into
"fatal".

## Reproduction

NixOS host, jailer enabled (default), any `boxlite run`. Confirmed the exact
bound set via `BOXLITE_DEBUG_PRINT_SEATBELT=1`:

```
/etc/ssl/certs                          (dir)
/etc/pki/tls/certs                      (dir)
/etc/ssl/certs/ca-certificates.crt      (symlink inside the 1st dir)  <-- conflict
/etc/pki/tls/certs/ca-bundle.crt        (symlink inside the 2nd dir)  <-- conflict
```

Minimal bwrap repro (nixpkgs bubblewrap 0.11.2):

```sh
# FAILS — dir then inner file, mirrors boxlite's bind order:
bwrap --ro-bind /nix /nix --proc /proc --dev /dev --tmpfs /tmp \
  --ro-bind /etc/ssl/certs /etc/ssl/certs \
  --ro-bind /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt \
  -- true
# bwrap: Can't create file at /etc/ssl/certs/ca-certificates.crt: No such file or directory

# OK — bind the dir only (the inner file is reachable through it):
bwrap --ro-bind /nix /nix --proc /proc --dev /dev --tmpfs /tmp \
  --ro-bind /etc/ssl/certs /etc/ssl/certs -- true
```

## Suggested fix

In `build_path_access` (or the `system_ca_paths` consumer), **drop any CA path
already covered by another CA path being bound** — i.e. don't bind a file if its
ancestor directory is also in the bind set. Minimal version: after collecting the
existing CA paths, filter out any path whose ancestor dir is also present.

That keeps the trust store readable (via the dir bind) and avoids the dangling
inner-symlink mount point entirely — fixing NixOS without affecting FHS distros.

## Environment

- boxlite 0.9.7
- NixOS (symlink-farm `/etc`; `/etc/ssl/certs/ca-certificates.crt` →
  `/etc/static/...` → `/nix/store/...-nss-cacert-*`)
- x86_64 + KVM, nixpkgs bubblewrap 0.11.2 on PATH
