# ferroamp

A small, fast music player for macOS in Rust, in the spirit of the classic
desktop players of the late 90s: three compact, dockable windows and no library.

- **Files, not a library.** Drop files or folders on it. Nothing is imported,
  moved or indexed. The playlist is a plain `.m3u8`. It opens M3U, M3U8 and
  PLS, including old playlists made on Windows: backslash paths, drive letters
  (the tracks are looked for beside the playlist) and pre-UTF-8 text.
- **CUE sheets.** A single-file album shows up as its separate tracks, with no
  gap between them. Open the `.cue`, or drop the folder it is in.
- **Light.** About 20 MB of memory and a 2.6 MB binary. While playing it measured
  1.5-4% of one core with the spectrum showing and under 1% with its windows
  covered. When idle, every thread sleeps. The visuals window adds up to about
  6% more, only while it is open and uncovered.
- **Internet radio.** Paste a station's address, or open its `.pls` or `.m3u`.
  It shows the station's name and the song that's on, including on the lock
  screen and in Control Center.
- **Yours.** No accounts, no subscriptions, no tracking. It only goes online to
  play a station you added.
- **Gapless**, including across a resample, so a 44.1 kHz album on a 48 kHz
  device has no gaps.
- **Visuals.** A spectrum or oscilloscope in the main window, plus a resizable,
  fullscreen window of beat-reactive feedback effects with ten original presets.
  Closed, it costs nothing.
- **Crossfade**, off unless you turn it on. Tracks of the same album stay
  gapless, and a station or a track you skip to is never faded.
- **ReplayGain** from existing tags, so loudness evens out with no scanning.
  Files without the tags play as they are.
- **Skins.** Drop a classic `.wsz` skin on it, or use right-click > Skin. None
  are included, so it only shows skins you bring. The built-in look is its own.
- 10-band equaliser with presets, media keys, and the macOS Now Playing panel.

Plays MP3, Ogg Vorbis, FLAC, WAV, AIFF, AAC/M4A, ALAC, MP2 and CAF.

## Build

Needs Rust (`brew install rustup && rustup default stable`). Rust from Homebrew's
`rustup` is not on your `PATH` by default. The script below finds it anyway.

```bash
scripts/bundle.sh
```

```bash
open dist/ferroamp.app
```

`scripts/bundle.sh --install` also copies the app to `~/Applications`. To use it
from Finder, choose **Open With > ferroamp**. It never claims file types for itself.

## Controls

| Key | Action | Key | Action |
|---|---|---|---|
| `Space` | play / pause | `z` `x` `c` `v` `b` | prev, play, pause, stop, next |
| `←` `→` | seek 5 s | `↑` `↓` | volume |
| `l` | add files | `s` / `r` | shuffle / repeat |
| `e` / `p` | show EQ / playlist | `+` / `-` | window size |
| `Delete` | remove selected | `Return` | play selected |
| `w` | show visuals | `n` | next visual preset |
| `f` or double-click | fullscreen visuals | `Esc` | leave fullscreen |
| `Cmd`+`L` or URL button | add a radio station | `Cmd`+`V` | paste stations or files |
| `j` | jump to file | `q` | play selected next |
| `i` | file info | `a` | always on top |
| `m` | collapse this window | | |

- Right-click anywhere for the menu, which also has crossfade and ReplayGain.
- `r` cycles repeat through off, all and one track.
- In jump to file, type part of a name or folder. `Return` plays, `Shift`+`Return`
  queues and `Esc` closes.
- Double-click a title bar, or use its small button, to collapse a window.
  Windows docked below it move up with it.
- Drag the main window and docked windows follow. Others snap to it when close.
- Click the time for remaining time. Click the small display to cycle through
  spectrum, oscilloscope and off.
- Visual presets change by themselves on a beat about every 30 seconds.
- Scroll over the main window for volume.
- Double-click a row to play it. Drag selected rows to reorder them. `Cmd` and
  `Shift` extend the selection, and `Shift`-click on SORT sorts by path.
- `Shift`-click eject adds a folder.

## How it works

```
 files -> Source -> Pipeline -> ring buffer -> CoreAudio callback -> speakers
          decode    resample   (2 s, lock-free)  EQ, volume
          (engine thread)                         |
                                                  v
                  UI thread <---- atomics, spectrum tap
```

- `engine/` has no UI dependency. The real-time callback takes no locks and makes
  no allocations.
- One monotonic sample counter drives everything. Markers say which track starts
  at which count, so a gapless boundary and a seek are the same mechanism, and
  the UI always shows what is audible rather than what has been decoded.
- `ui/visuals.rs` warps each frame into the next on a coarse grid and adds new
  light from the audio. A preset is a warp function, a fade and a draw function.
- `ui/` draws to a 275-wide software canvas, and the GPU scales it up with
  nearest-neighbour filtering. Redraws only happen when something changed, and
  the event loop truly sleeps when idle.

## Tests

```bash
cargo test
```

Fixture tests generate real encoded files with `ffmpeg` and check gapless joins,
seeking and tags sample by sample. They skip themselves if `ffmpeg` is missing.
Window images land in `target/ui-snapshots`. To exercise the real audio device
at zero volume:

```bash
cargo test --test device -- --ignored --test-threads=1
```

## Known limits

- **Opus** is not supported, because the decoding library has no Opus decoder.
- **Radio:**
  - MP3, AAC and Ogg Vorbis stations work, over HTTP or HTTPS.
  - Stations that only offer HLS (`.m3u8` segment lists, as some broadcasters
    use) are not supported, and neither are Opus stations.
  - Low-bitrate "AAC+" (HE-AAC) stations play, but without their top
    frequencies, so they sound duller than they should.
  - A song title can change a few seconds before the song does, because the
    player buffers ahead.
  - If a station drops, the player reconnects. After five failed tries in a
    row it moves on to the next item.
- **Titles:** the bitmap font covers ASCII and folds accented Latin letters to
  their base letter (Ż shows as Z). macOS draws other scripts at the same small
  size. Cyrillic and kana read well, but dense characters such as kanji and
  Hangul are blurry. Jump to file takes typed letters but not an input method,
  so paste Japanese or Chinese into it with `Cmd`+`V`.
- **Skins:** windows stay rectangular, so a skin's cut-out shape is ignored, and
  the playlist uses the app's own font rather than the one a skin names. The EQ's
  auto button does nothing.
- **Gapless needs the file's help.** MP3 needs the usual LAME header, which
  nearly all encoders write. AAC in M4A may keep a tiny gap, as edit lists are
  not fully applied.
- Editing the playlist or the queue in the last 2 s of a track, plus the
  crossfade length if that is on, can't change what has already been lined up
  as next.

Settings and the session playlist live in `~/Library/Application Support/ferroamp`.

## License

ferroamp is licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE)
or the [MIT license](LICENSE-MIT), at your option. Unless you say otherwise, any
contribution you submit is licensed the same way, with no extra terms.

The artwork is drawn in code for this app. The bitmap font is partly based on the
classic 5x7 font from the Adafruit GFX Library; see [THIRD_PARTY.md](THIRD_PARTY.md).
ferroamp is an independent project, not affiliated with any other player or company.
