# aura

A compatibility layer that runs an Xbox Series X|S title natively on
Windows, from files the user supplies. Work in progress; nothing here runs
the title yet.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

Written from scratch, with ideas and (where GPL terms require, attribution)
code inspired by the VIctory project and its forks, XCL and similar layers.
See `docs/architecture.md` for the design, which targets low memory use and
throughput first.

## Layout

| Path | What |
| --- | --- |
| `apps/aura` | the executable (run, inspect, translate) |
| `crates/aura-loader` | maps the title's PE images, runs its x86-64 code in-process |
| `crates/aura-hle` | the console's OS surface (kernel, runtime, storage, input, audio) |
| `crates/aura-gpu` | the console's D3D12.X on host D3D12; ownership of title memory |
| `crates/aura-shader` | console GPU code to DXIL, ahead of time where possible |
| `crates/aura-formats` | the title's archive, texture and audio container formats |
| `crates/aura-core` | shared plumbing (config, logging, clocks) |

## License

GPL-3.0-or-later. See LICENSE.
