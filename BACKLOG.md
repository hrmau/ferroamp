# Backlog

Ideas parked for later. Sizes are rough estimates.

## Sound and formats

| Feature | Notes | Size |
|---|---|---|
| Output device picker | Playback only follows the system default output today. | Small |
| Opus | Files and radio. Needs a decoder dependency, as the current decoding library has none. | Medium |
| HLS radio | Stations that only offer `.m3u8` segment lists. | Medium |

## Classic player features

| Feature | Notes | Size |
|---|---|---|
| Tracker modules (MOD, XM, S3M, IT) | Probably means bundling libopenmpt, which would grow the app noticeably. | Medium |
| Station directory | Browse radio through the open radio-browser.info, with no account. The app would then go online for more than playing a station the user added. | Medium |

## Follow-ups

| Feature | Notes | Size |
|---|---|---|
| Tag editing | The file info panel is read-only. | Medium |
| Shaped skins | Honour a skin's `region.txt` so windows can be non-rectangular. | Medium |

## Decided against

Recorded so they aren't reconsidered without a new reason.

- **Media library, CD ripping, video:** bloat.
- **Real MilkDrop presets:** they need an expression engine and shaders, and would cost far more CPU and GPU than the current visuals.
- **Plugin system:** old plugins are Windows-only, so the API would have nothing to run.
- **Last.fm scrobbling:** needs an account.
