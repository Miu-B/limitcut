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
- **PullToOBS JSON input** — process a single metadata JSON or a whole
  directory of them, automatically resolving the replay buffer, full recording,
  job, encounter title, and output filename. In `--json-dir` mode, the title
  overlay is auto-generated as `"<encounter>/<job> POV"` with optional
  user-provided lines appended.
- **InstaPost support** — process single-file `instapost_*.json` descriptors,
  resolve the replay buffer clip relative to the JSON file, transcode it to
  MP4, and archive it under a deterministic `instapost/YYYY-MM-DD/...` layout.
- **InstaPost watch mode** — monitor an OBS directory, process existing backlog
  on startup, ignore `*.tmp`, and move processed descriptors into `done/` or
  `failed/`.
- **Optional Discord webhook upload** — after InstaPost processing succeeds,
  upload the MP4 to a Discord webhook configured in `~/.config/limitcut/config.toml`.
  InstaPost can encode directly as **720p** or **1080p**, and the produced file is
  the one that gets uploaded.
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

Normal mode:

```text
limitcut <PRE_VIDEO> <POST_VIDEO> [OPTIONS]
```

PullToOBS JSON mode:

```text
limitcut --json <FILE> [OPTIONS]
limitcut --json-dir <DIR> [OPTIONS]
```

InstaPost mode:

```text
limitcut --instapost <FILE> [OPTIONS]
limitcut --watch-instapost <DIR> [OPTIONS]
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

Specify a base output directory while keeping the auto-generated filename:

```bash
limitcut prepull.mkv pull.mkv --output-dir ~/Recordings/FFXIV
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

Process a single PullToOBS JSON and place the output under an encounter folder:

```bash
limitcut --json ~/Videos/OBS/2026-04-24_07-55-31.json --output-dir ~/Recordings/FFXIV
```

This produces an output like:

```text
~/Recordings/FFXIV/2026-04-24/Deltascape_V1.0/BLM/07-55-31.mp4
```

Process all PullToOBS JSON files in a directory:

```bash
limitcut --json-dir ~/Videos/OBS --output-dir ~/Recordings/FFXIV
```

Process a single InstaPost descriptor:

```bash
limitcut --instapost ~/Videos/OBS/instapost_20260602_190702.json --output-dir ~/Recordings/FFXIV
```

Watch an OBS directory for InstaPost backlog and new arrivals:

```bash
limitcut --watch-instapost ~/Videos/OBS --output-dir ~/Recordings/FFXIV
```

In `--json-dir` mode, the title overlay is auto-generated as
`"<encounter>/<job> POV"`. InstaPost uses the same encounter/territory label
for archive paths and Discord post text, but does not render title overlays,
so `--title` does not apply there.

> **Note:** The standard PullToOBS encounter JSON metadata output is available
> from [PullToOBS](https://github.com/Miu-B/PullToOBS) v0.3.1.0 onward.
> The InstaPost / Quick Save descriptor flow used by `--instapost` and
> `--watch-instapost` is available from PullToOBS v0.4.0.0 onward.
> Older versions do not produce these JSON files, so the corresponding
> limitcut modes won't apply.

### Options

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path (default: `<pre_video>_combined.mp4`) |
| `--output-dir <DIR>` | Base output directory. In JSON mode, output is organised as `<dir>/YYYY-MM-DD/<encounter>/<job>/HH-MM-SS.mp4` |
| `--json <FILE>` | Process a single PullToOBS metadata JSON file |
| `--json-dir <DIR>` | Process all `*.json` PullToOBS metadata files in a directory |
| `--instapost <FILE>` | Process a single InstaPost JSON descriptor |
| `--watch-instapost <DIR>` | Watch a directory for `instapost_*.json` descriptors |
| `--overwrite` | Overwrite the output file if it exists |
| `--encoder <ENCODER>` | H.264 encoder: `nvenc`, `vaapi`, `videotoolbox`, `libx264` (ignored in InstaPost mode, which uses a Discord-oriented profile) |
| `--blur <x:y:w:h>` | Blur a rectangular region (repeatable) |
| `--preview-blur [SECS]` | Render a single frame with blur regions applied (default: 1.0s) |
| `--dry-run` | Print the ffmpeg command and exit |
| `--fadein [SECS]` | Fade-in duration from black at the start (default: 1.0s; ignored in InstaPost mode) |
| `--fadeout [SECS]` | Fade-out duration to black at the end (default: 1.0s; ignored in InstaPost mode) |
| `--black-hold <SECONDS>` | Seconds of black screen before the fade-in begins (ignored in InstaPost mode) |
| `--title <TEXT>` | Centred title text during black-hold/fade-in. Use `/` for line breaks. In `--json-dir` mode, `"<encounter>/<job> POV"` is auto-prepended. Ignored in InstaPost mode |
| `-v, --verbose` | Enable debug logging |
| `-h, --help` | Show help |
| `-V, --version` | Show version |

Discord upload / InstaPost output config keys:

```toml
discord_enabled = true
discord_webhook_url = "https://discord.com/api/webhooks/..."
# Optional: "720p" (default) or "1080p"
# discord_video_mode = "720p"
```

InstaPost uses a Discord-oriented H.264 encode profile directly, so `720p`
and `1080p` choose both the output resolution and the compression profile used
for the saved MP4 and the uploaded file.

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

In InstaPost mode, limitcut instead:

1. Validates the dedicated InstaPost JSON schema.
2. Resolves `replay_buffer` relative to the JSON file.
3. Processes the single replay-buffer clip directly — no overlap detection and
   no second `recording` file required.
4. Applies configured blur regions, but skips fade-in, fade-out, black-hold,
   and title overlay so short clips are not shortened further.
5. Encodes directly to the configured Discord video mode (`720p` by default,
   or `1080p` when configured) using a Discord-oriented compression profile.
6. Writes the final video as
   `<dir>/instapost/YYYY-MM-DD/<display_label>/<job>/HH-MM-SS.mp4`.
7. On successful processing, moves the InstaPost JSON descriptor into `done/`;
   on failure, into `failed/` (watch mode and normal `--instapost` runs).
   `--dry-run` and `--preview-blur` leave the descriptor in place.
8. Optionally uploads that same MP4 to a configured Discord webhook.

In JSON mode, limitcut additionally:

1. Validates the PullToOBS JSON structure before any processing starts.
2. Resolves `recording` and `replay_buffer` relative to the JSON file.
3. Normalizes the `encounter` and `job` names into safe folder names.
4. Writes the final video as
   `<dir>/YYYY-MM-DD/<encounter>/<job>/HH-MM-SS.mp4`.
5. In `--json-dir` batch mode, auto-generates the title overlay as
   `"<encounter>/<job> POV"` (user-provided `--title` lines are appended).

The normalised correlation score must be at least 0.3 — if it's below that,
limitcut aborts with a clear error instead of producing a bad output.

## Acknowledgements

The organised directory tree output structure (`YYYY-MM-DD/<encounter>/<job>/`)
was suggested by Alyssa Claude.

## License

[MIT](LICENSE)
