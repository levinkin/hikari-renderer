# hikari renderer

the hikari renderer draws a live2d model and streams the frames over http or shows it in its own window. i built it to be as light weight as possible without sacrificing speed or quality. it runs on metal on mac and vulkan on windows and linux.

[getting started](docs/getting-started.md) has setup, running it and the http api.

## numbers

### against vtube studio

both apps ran one at a time on the same windows pc (rtx 4090), fullscreen at 2560x1440 and capped at 60 fps, with the two sample models vtube studio ships. active means an action every 2 seconds plus lip sync. each number is the median of three 60 second runs.

| hiyori | vtube studio | hikari renderer | hikari renderer uses |
|---|---|---|---|
| memory, idle / active | 728 / 732 MB | 147 / 159 MB | 80% / 78% less |
| private memory, idle / active | 1,571 / 1,581 MB | 311 / 348 MB | 80% / 78% less |
| gpu memory, idle / active | 711 / 711 MB | 197 / 218 MB | 72% / 69% less |
| cpu per frame, idle / active | 8.4 / 8.2 ms | 0.45 / 0.69 ms | 95% / 92% less |
| gpu per frame, idle / active | 0.23 / 0.26 ms | 0.28 / 0.39 ms | 22% / 49% more |

| akari | vtube studio | hikari renderer | hikari renderer uses |
|---|---|---|---|
| memory, idle / active | 758 / 769 MB | 148 / 165 MB | 81% / 79% less |
| private memory, idle / active | 1,632 / 1,658 MB | 321 / 364 MB | 80% / 78% less |
| gpu memory, idle / active | 754 / 755 MB | 209 / 233 MB | 72% / 69% less |
| cpu per frame, idle / active | 10.7 / 9.4 ms | 0.44 / 0.77 ms | 96% / 92% less |
| gpu per frame, idle / active | 0.99 / 0.99 ms | 0.28 / 0.38 ms | 72% / 62% less |

the exe and core dll come to 1.6 MB against vtube studio's 1,548 MB install. gpu time comes from windows' busy counter, so treat it as an estimate. the hikari renderer draws with 4x msaa, which probably explains its slightly higher gpu time on hiyori.

### quality

against live2d's own renderer, which this project used before, frames differ by 0.3 to 0.6 out of 255 on average, mostly along edges that msaa smooths. the mac and windows builds differ from each other by about 0.1. i don't have a clean comparison against vtube studio, because its framing can't be matched exactly.

### on a mac

vtube studio's own hiyori, both apps fullscreen on a 4k display (3840x2160) at a 60 fps cap, on an apple m6. active means the same three hotkey animations every 2 s and the same lip sync on both sides. each number is the median of three 60 s runs.

| | hikari renderer | vtube studio | saved |
|---|---|---|---|
| fps, idle | 60.5 | 58.0 | |
| memory, idle | 148 MB | 1,758 MB | 92% |
| cpu per frame, idle | 1.20 ms | 11.31 ms | 89% |
| fps, active | 60.4 | 58.0 | |
| memory, active | 179 MB | 1,773 MB | 90% |
| peak memory, active | 180 MB | 1,946 MB | 91% |
| cpu per frame, active | 2.05 ms | 11.74 ms | 83% |

the hikari renderer's binary is 1.1 MB and live2d's core dylib adds 0.27 MB, against 1.9 GB for vtube studio.

## licensing

my code is under the mit license (see `LICENSE`), and bc7enc in `native/engine/bc7enc/` keeps its own. this repo has no live2d files in it: you download the cubism sdk yourself, and its core is under live2d's proprietary license and its framework under their open software license. if you ship or sell something built on it, you need a license from live2d, see https://www.live2d.com/sdk/license/. models aren't included, and each has its own terms, so check them before you use one.
