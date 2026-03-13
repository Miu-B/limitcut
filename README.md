# limitcut

Seamlessly combine two overlapping video recordings into a single MP4.

limitcut takes a **pre-video** (like a replay buffer clip) and a **post-video**
(the full recording that starts slightly before the pre-video ends), finds where
they overlap using audio cross-correlation, and stitches them together with
ffmpeg — no manual trimming needed.

## How it was born

limitcut started life as the perfect companion to
[PullToOBS](https://github.com/Miu-B/PullToOBS), a Dalamud plugin for FFXIV
that automates OBS recording around boss pulls. PullToOBS saves a replay buffer
clip right before the pull and starts a fresh recording immediately after — but
that leaves you with two files that overlap by a few seconds. limitcut closes
that gap automatically, so you get one clean video every time.

That said, there's nothing game-specific about limitcut itself. Any pair of
overlapping recordings with shared audio will work just fine.

## Features

- **Audio cross-correlation** — finds the exact overlap point by comparing
  waveforms, independent of absolute volume levels.
- **Hardware-accelerated encoding** — auto-detects the best available H.264
  encoder (NVENC, VAAPI, VideoToolbox) and falls back to libx264.
- **Blur regions** — optionally blur rectangular areas of the output (e.g. UI
  elements, names) with a repeatable `--blur x:y:w:h` flag.
- **Blur preview** — `--preview-blur` renders a single JPEG frame with your
  blur regions applied so you can verify placement before running the full encode.
- **Dry-run mode** — `--dry-run` prints the exact ffmpeg command without
  running it.
- **Progress bar** — shows real-time encoding progress.
- **Cross-platform** — runs on Linux, Windows, and macOS.

## Requirements

- [ffmpeg](https://ffmpeg.org/) (must be in `PATH` — both `ffmpeg` and
  `ffprobe` are used)

## Installation

### From GitHub Releases

Download the latest binary for your platform from the
[Releases](https://github.com/Miu-B/limitcut/releases) page.

### From source

```bash
cargo install --git https://github.com/Miu-B/limitcut.git
```

Or clone and build locally:

```bash
git clone https://github.com/Miu-B/limitcut.git
cd limitcut
cargo build --release
# Binary is at target/release/limitcut
```

## Usage

```
limitcut <PRE_VIDEO> <POST_VIDEO> [OPTIONS]
```

### Examples

Basic usage — produces `prepull_combined.mp4` alongside the input:

```bash
limitcut prepull.mkv pull.mkv
```

Specify an output path:

```bash
limitcut prepull.mkv pull.mkv -o combined.mp4
```

Blur two regions of the video (e.g. a chat box and a name plate):

```bash
limitcut prepull.mkv pull.mkv --blur 0:840:480:200 --blur 1400:0:480:60
```

Force a specific encoder and preview the ffmpeg command:

```bash
limitcut prepull.mkv pull.mkv --encoder libx264 --dry-run
```

Preview blur region placement (saves a JPEG frame):

```bash
limitcut prepull.mkv pull.mkv --blur 0:840:480:200 --blur 1400:0:480:60 --preview-blur
```

Preview at a specific timestamp (e.g. 12.5 seconds in):

```bash
limitcut prepull.mkv pull.mkv --blur 0:840:480:200 --preview-blur 12.5
```

### Options

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path (default: `<pre_video>_combined.mp4`) |
| `--overwrite` | Overwrite the output file if it exists |
| `--encoder <ENCODER>` | H.264 encoder: `nvenc`, `vaapi`, `videotoolbox`, `libx264` |
| `--blur <x:y:w:h>` | Blur a rectangular region (repeatable) |
| `--preview-blur [SECS]` | Render a single frame with blur regions applied (default: 1.0s) |
| `--dry-run` | Print the ffmpeg command and exit |
| `-v, --verbose` | Enable debug logging |
| `-h, --help` | Show help |
| `-V, --version` | Show version |

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Invalid input (bad paths, bad arguments) |
| `2` | Processing failure (ffmpeg error, no audio overlap detected) |

## How it works

1. **Probe** the pre-video duration with ffprobe.
2. **Extract** the last ~6 seconds of the pre-video audio and the first 0.5
   seconds of the post-video audio as raw PCM.
3. **Slide** the short needle over the haystack using normalised
   cross-correlation to find the best match.
4. **Trim** the pre-video at the detected cut point and concatenate it with the
   full post-video using ffmpeg's `concat` filter.

The normalised correlation score must be at least 0.3 — if it's below that,
limitcut aborts with a clear error instead of producing a bad output.

## License

[MIT](LICENSE)
