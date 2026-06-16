# Contributing

## Prerequisites

- Nix with flakes enabled
- direnv with nix-direnv recommended

## Setup

```sh
direnv allow
```

Or manually:

```sh
nix develop
```

## Checks

```sh
just ci
```

For focused checks:

```sh
just fmt-check
just check
just clippy
just test
```
