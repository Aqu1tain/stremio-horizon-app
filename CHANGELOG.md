# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com),
and this project adheres to [Semantic Versioning](https://semver.org).

## [Unreleased]

## [0.3.3] - 2026-07-31

### Added

- CI workflow running `cargo check` and `cargo test` on Linux and Windows for every pull request

### Changed

- Updated frontend to stremio-horizon v0.1.5
- Release workflow now builds stremio-service from the pinned fork tag `v0.1.0-horizon.2` instead of
  whatever `master` happens to be, with a `service_version` input to override it
- Release workflow marks a release as a pre-release when the tag carries a suffix, so release
  candidates no longer publish as stable and are skipped by the updater

### Fixed

- External requests lost their query string when the proxy forwarded them, because the target URL
  was read from the path with the query already split off. A remote streaming server then answered
  500 to every `hlsv2/probe` call, since `mediaURL` never arrived, and playback failed with "Video
  is not supported" on every stream. Present since the same-origin proxy landed in 0.2.1; only
  visible when the streaming server is not on localhost, which is why the bundled service hid it
- Fullscreen toggle no longer goes stale when the window leaves fullscreen without going through
  the app — the green button, Cmd+Ctrl+F, the Window menu. The bridge kept a local mirror that only
  moved when the frontend called it; it now re-reads the real window state, retrying until the macOS
  transition settles (measured: the window still reports fullscreen ~150ms in, and only reads
  correctly around 600ms)

## [0.3.2] - 2026-05-26

### Changed

- Updated frontend to stremio-horizon v0.1.4
- Update checks now keep the pending update object until the user installs it
- Added a Tauri-only auto-update setting and manual pending-update recovery in the banner
- Reuse TCP connections and threads in proxy server (connection pooling via ureq agents, threadpool instead of raw thread::spawn)

### Fixed

- Install now uses the already-discovered update instead of randomly rechecking
- Restart after update install was verified in a packaged macOS app
- Cache hashed frontend assets with `immutable` headers; force revalidation only on the root entry point (#36)
- Surface config save errors instead of silently discarding them, so settings can no longer be lost without warning (#37)
- Cap Chromecast message size at 8MB to prevent malicious receivers triggering 4GB allocations (#35)
- Drop guard for mDNS daemon so failed Chromecast discovery no longer leaks daemons (#35)
- Catch panics in Chromecast session thread and emit NOT_CONNECTED state instead of zombie-ing (#35)
- Use `serde_json::json!` for Chromecast LAUNCH/STOP/state payloads instead of manual `format!` (#35)
- Kill stremio-service on app exit instead of leaving it running (#16)
- Show warning banner when streaming service fails to start instead of silently continuing
- Upstream check workflow now reads upstreamVersion from horizon package.json instead of comparing mismatched release tags

## [0.3.1] - 2026-03-12

### Changed

- Updated frontend to stremio-horizon v0.1.3

## [0.3.0] - 2026-03-11

### Added

- Auto-update mechanism with signed releases
- x64 macOS build to release workflow
- macOS quarantine fix documentation

### Changed

- Redesigned settings page (stremio-horizon v0.1.2)
- Release marked as stable (non-prerelease)

### Fixed

- Numeric pre-release version for MSI compatibility

## [0.2.2] - 2026-02-12

### Fixed

- Disable hardware acceleration on Linux to prevent EGL crash

### Changed

- Download web build from stremio-horizon release instead of building from source
- Add RPM to Linux build artifacts

## [0.2.1] - 2026-02-09

### Added

- Proxy external addon requests via same-origin fetch interceptor

### Fixed

- Resolve fullscreen bridge timing issue on WebView2
- Preserve invoke context and exclude Tauri IPC from fetch interceptor
- Clean stale stremio-service clone before checkout in CI

## [0.2.0] - 2026-02-09

### Changed

- Replace localhost plugin with same-origin proxy server
- Enable TLS for proxy, add fullscreen bridge, refactor lib.rs

## [0.1.1] - 2026-02-09

### Fixed

- Disable DMABUF renderer on Linux to prevent EGL crash

### Changed

- Add Rust build cache for faster CI builds

## [0.1.0] - 2026-02-09

### Added

- Initial Tauri desktop wrapper for Stremio Horizon
- Stremio icon, graceful sidecar spawn, resource bundling
- CI release workflow for macOS, Linux, and Windows
- LICENSE and CONTRIBUTING

### Fixed

- Use fixed port to persist session across restarts
- Bypass CORS for stremio-service communication

[Unreleased]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/Aqu1tain/stremio-horizon-app/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Aqu1tain/stremio-horizon-app/releases/tag/v0.1.0
