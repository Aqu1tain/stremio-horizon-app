# Stremio Horizon App

Desktop app for [Stremio Horizon](https://github.com/Aqu1tain/stremio-horizon), built with [Tauri](https://tauri.app).

Bundles the Stremio Horizon web UI with a [forked stremio-service](https://github.com/Aqu1tain/stremio-service) for local streaming.

> This is not a replacement for [Stremio](https://www.stremio.com) — just a personal take on what a great UI could look like. Go star Stremio!

## Download

| Platform | Download |
|----------|----------|
| macOS (Apple Silicon) | [.dmg](https://github.com/Aqu1tain/stremio-horizon-app/releases/latest) |
| Linux (x64) | [.AppImage / .deb](https://github.com/Aqu1tain/stremio-horizon-app/releases/latest) |
| Windows (x64) | [.msi / .exe](https://github.com/Aqu1tain/stremio-horizon-app/releases/latest) |

## How it works

```
┌──────────────────────────────┐
│     Stremio Horizon App      │
│          (Tauri)             │
│                              │
│  ┌────────────────────────┐  │
│  │   Stremio Horizon UI   │  │
│  │     (web frontend)     │  │
│  └────────────────────────┘  │
│                              │
│  ┌────────────────────────┐  │
│  │    stremio-service      │  │
│  │  (streaming backend)   │  │
│  └────────────────────────┘  │
└──────────────────────────────┘
```

The Tauri shell serves the frontend over `localhost` and spawns `stremio-service` as a sidecar process. The forked service has CORS disabled and auto-update removed so it works seamlessly inside the app.

## Development

### Prerequisites

- [Rust](https://rustup.rs)
- [pnpm](https://pnpm.io) 10+
- [Node.js](https://nodejs.org) 20+
- [Stremio Horizon](https://github.com/Aqu1tain/stremio-horizon) cloned as a sibling directory (`../stremio-web`)

### Setup

1. Clone the repo:

```bash
git clone https://github.com/Aqu1tain/stremio-horizon-app
cd stremio-horizon-app
pnpm install
```

2. Place stremio-service binaries in `src-tauri/binaries/`:

Download from [stremio-service releases](https://github.com/Aqu1tain/stremio-service/releases) or build from source. You need: `stremio-service`, `server.js`, `stremio-runtime`, `ffmpeg`, `ffprobe`.

3. Run in development mode:

```bash
pnpm tauri dev
```

### Build

```bash
pnpm build
```

Produces a `.dmg` (macOS), `.AppImage` / `.deb` (Linux), or `.msi` / `.exe` (Windows) in `src-tauri/target/release/bundle/`.

## Related repos

- [Stremio Horizon](https://github.com/Aqu1tain/stremio-horizon) — frontend
- [stremio-service (fork)](https://github.com/Aqu1tain/stremio-service) — streaming backend

## Credits

Built on top of [stremio-service](https://github.com/Stremio/stremio-service) by [Smart Code](https://www.stremio.com) and [stremio-web](https://github.com/Stremio/stremio-web). Original code is licensed under GPL-2.0.

## License

GPL-2.0 — see [LICENSE.md](LICENSE.md).
