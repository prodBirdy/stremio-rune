# stremio-rune

A native Windows and macOS client for [Stremio](https://www.stremio.com), built with the
[Rune](https://github.com/anomalyco/rune) UI framework. It uses your Stremio account and add-ons,
and plays through Stremio's own streaming server and mpv, so everything you have in Stremio
works here. The UI is a dark, cinematic one in plain Rust, at 120 Hz.

> **Alpha.** It works day to day on Windows 11, but expect rough edges. The macOS port is newer.

## Features

- **Your account:** sign in with your Stremio account, or reuse the session of the installed
  Stremio app. Your add-ons, library and Continue Watching come from Stremio's servers, and
  watch progress is written back, so it stays in sync with the official apps.
- **Home:** a hero carousel, then Up Next and each add-on catalog as a row you can page left
  and right; more titles load as you go.
- **Browse and search:** Movies and Shows per catalog; search across every add-on that
  supports it.
- **Titles:** seasons and episodes, and streams from all your add-ons in a side sheet.
- **Player:** mpv renders on the GPU straight into the app (hardware decoding, HDR tone
  mapping, mpv's scalers, subtitles), with no copy through the CPU: into shared Direct3D
  textures on Windows, IOSurfaces on macOS. Controls float over the
  video and hide with the cursor when you're idle. Includes audio track and subtitle choice
  (embedded or from add-ons), a quality switch between the title's streams at the current
  position, volume, and fullscreen.
- **Add-ons:** see installed add-ons, install from a URL or the community list, configure,
  and uninstall.
- **Settings:** player defaults (languages, subtitle style, seek step, hardware decoding)
  and Stremio's streaming-server options.
- **Lean:** images are fetched at the size they're shown and kept within a memory budget,
  and the app draws nothing while idle in the background.

## Requirements

- Windows 10 or 11 with a Direct3D 12 GPU, or macOS 13 or later.
- [Stremio](https://www.stremio.com/downloads) installed (on macOS, Stremio 5 in
  `/Applications`). The app uses the `libmpv` and the streaming server
  that Stremio ships. It starts the streaming server itself when Stremio isn't running, and
  stops it again on exit. `MPV_LIB` points it at another libmpv.
- A Stremio account.

## Build

Windows:

```sh
cargo build --release
target\release\stremio-rune.exe
```

macOS:

```sh
scripts/bundle-macos.sh
open "target/release/Stremio Rune.app"
```

The script builds a release binary and wraps it as `Stremio Rune.app` (icon, Info.plist,
ad-hoc signature); copy it to `/Applications` to keep it. `cargo run --release` works too.

Needs a current stable Rust (edition 2024). Video playback on Windows needs a Windows import
path for Rune's `NativeSurface`; until that lands upstream, `Cargo.toml` patches Rune with the
branch that adds it. macOS uses Rune's own IOSurface import.

`STREMIO_RUNE_VIDEOTEST=<video>` opens a window that only plays that file through the GPU path
and writes frame and CPU counters to `stremio-rune-videotest.txt` in the temp folder.

## Status and limits

- Windows: the player, the window frame and fullscreen use Win32 and Direct3D.
- macOS: the window keeps its traffic lights over the app's title strip, and fullscreen is
  macOS's own (F11, `f` or the green button). Signing in reuses the Stremio app's session when
  it finds one in Stremio's WebKit storage.
- HDR is tone-mapped by mpv, which looks slightly different from mpv's own window output.
- Not affiliated with or endorsed by Stremio. "Stremio" is a trademark of its owners.

## License

MIT, see [LICENSE](LICENSE).
