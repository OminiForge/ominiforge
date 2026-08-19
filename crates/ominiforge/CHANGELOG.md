# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/OminiForge/ominiforge/compare/ominiforge-v0.1.1...ominiforge-v0.2.0) - 2026-08-19

### Added

- *(core,net)* file-tree browsing API + injectable session provider

### Other

- drop unused deps and fix formatting after view-layer removal
- [**breaking**] remove pre-rendered view layer, status hub, net crate, and gateway server
- point README and crate comments at runtime-architecture
- remove dead eval/evolution and GPUI build config
- [**breaking**] remove GPUI/Web UI (zero-UI pivot)
- speed up CI and add Nix/English/design static lints ([#37](https://github.com/OminiForge/ominiforge/pull/37))
- *(deps)* bump tower-http from 0.6.11 to 0.7.0 ([#29](https://github.com/OminiForge/ominiforge/pull/29))
- *(deps)* bump similar from 2.7.0 to 3.1.2 ([#28](https://github.com/OminiForge/ominiforge/pull/28))

## [0.1.1](https://github.com/OminiForge/ominiforge/compare/v0.1.0...v0.1.1) - 2026-08-10

### Other

- *(deps)* bump sha2 from 0.10.9 to 0.11.0 ([#30](https://github.com/OminiForge/ominiforge/pull/30))
- *(deps)* bump toml from 0.8.23 to 1.1.4+spec-1.1.0 ([#27](https://github.com/OminiForge/ominiforge/pull/27))
- drop unused deps orphaned by the CLI split
