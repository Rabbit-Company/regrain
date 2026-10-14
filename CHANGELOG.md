# Changelog

Error codes, JSON field names and the film grain table format stay compatible within a major version. Messages meant for people may change in any release.

## 0.1.0 - 2026-10-14

First release.

### Commands

- `diff` measures the grain a denoiser removed and writes a film grain table, one entry per scene.
- `apply` writes a table into an AV1 video, with `--verify` to read the result back and decode it with dav1d before keeping it.
- `export` reads the table out of an AV1 video.
- `remove` takes film grain synthesis out of an AV1 video.
- `stats` shows every table entry with the grain strength a decoder produces.
- `completions` and `manual` print shell completions and the manual page.

### Measuring

- Native bit depth, multithreaded, with `--scan full`, `half`, `quarter` and `eighth`.
- Grain strength is the mean block strength per brightness level, leaving out blocks far above the median.
- `--output-depth` sets the bit depth of the encode the grain goes into, so rounding in the decoder is accounted for.
- Chroma grain is measured only where luma is flat as well.
- Flat blocks use a fixed limit. Scenes with too few of them, such as foliage, also use loosely flat blocks.
- The denoised video can come from standard input as y4m.
- Short intra only videos are split into more chunks, so every thread has work.
- The two videos are checked frame by frame and `diff` stops with `misaligned` when they are off by a frame.
- Videos with a variable frame rate are refused with `variable_frame_rate`.
- `--crop`, `--start` and `--duration` measure part of a video without making copies.

### Writing grain

- Frames that share parameters with a reference frame point at it instead of repeating them.
- Every frame gets the grain of the time it is shown, also when it is decoded earlier.
- Tables from any tool are made to follow the AV1 rules for the video's format.
- Tables written by libaom with entries that reuse earlier parameters are read correctly.
- An empty table gives the input without grain.
- IVF, low overhead OBU streams and Annex B are read and written directly. `--fps` sets the frame rate for streams without timestamps.
- MKV, WebM and MP4 are muxed from a finished video file, so ffmpeg interleaves the tracks by time.
- Outputs and tables are written under a temporary name, so a failed write leaves nothing behind.

### Output

- `--progress json` writes one JSON object per line, `-q` prints only errors.

### Project

- MIT licence.
- Every failure has a stable error code. Cancelling with Ctrl+C or SIGTERM stops ffmpeg, removes partial output and exits with code 130.
