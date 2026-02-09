# Stremio Horizon App

Desktop wrapper for [Stremio Horizon](https://github.com/Aqu1tain/stremio-horizon), built with [Tauri](https://tauri.app).

Bundles the Stremio Horizon web UI with [stremio-service](https://github.com/Stremio/stremio-service) as a sidecar for streaming.

## Prerequisites

- [Rust](https://rustup.rs)
- [pnpm](https://pnpm.io)
- [Stremio Horizon](https://github.com/Aqu1tain/stremio-horizon) cloned as a sibling directory (`../stremio-web`)

## Setup

Download the stremio-service binary for your platform from [releases](https://github.com/Stremio/stremio-service/releases) and place it in `src-tauri/binaries/` with the target triple suffix:

```bash
# macOS Apple Silicon
cp stremio-service src-tauri/binaries/stremio-service-aarch64-apple-darwin

# macOS Intel
cp stremio-service src-tauri/binaries/stremio-service-x86_64-apple-darwin

# Windows
cp stremio-service.exe src-tauri/binaries/stremio-service-x86_64-pc-windows-msvc.exe
```

## Development

```bash
pnpm install
pnpm tauri dev
```

## Build

```bash
pnpm build
```

Produces a `.dmg` (macOS), `.exe` (Windows), or `.AppImage` (Linux) in `src-tauri/target/release/bundle/`.

## License

GPL-2.0
