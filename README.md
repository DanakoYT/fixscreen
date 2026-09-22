# FixScreen

Part of my monitor just died — no picture on that section. Not ready to replace
it yet, so I wrote this instead: it grabs the whole desktop and draws a scaled-down
copy of it in a small always-on-top window. Whatever was supposed to show up on
the dead part of the screen, you can now see in that window.

Capture is done via DXGI Desktop Duplication, with a GDI BitBlt fallback for
games/apps that don't hand over frames through DXGI (exclusive fullscreen, mostly).

## Features

- always-on-top window
- resize with the mouse wheel, drag from anywhere to move it
- switch between monitors with `M`
- single .exe, nothing to install

## Controls

| Action | Effect |
|---|---|
| Mouse wheel | resize the window |
| Left-click + drag | move the window |
| `M` | next monitor |
| `Esc` / right-click / the X in the corner | close |

## Building

Needs Rust with the MSVC toolchain (default on Windows via [rustup](https://rustup.rs/)):

```
cargo build --release
```

The binary shows up at `target\release\fixscreen.exe` — no install step.

Only dependency is the `windows` crate (Win32/D3D11/DXGI bindings) — everything
it actually calls into ships with Windows itself.

## Why not UltraMon / Actual Multiple Monitors / DisplayFusion

They all have this feature (mirror part of the desktop into a window), buried
inside a paid all-in-one monitor manager with a hundred other features you don't
need. This is one button, one .exe, free.

## About the code

I don't know C++ or Rust myself — this whole thing was built through
conversations with Claude (Anthropic): I described what I needed, tested it,
reported bugs, and it wrote and fixed the code. Started as C++, later ported
to Rust (this repo is the Rust version).

## License

MIT.
