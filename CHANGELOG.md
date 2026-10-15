# Changelog

Error codes, JSON field names and the film grain table format stay compatible within a major version. Messages meant for people may change in any release.

## 0.2.1 - 2026-10-15

### Fixes

- `diff` could write a scene with 15 luma scaling points, one more than AV1 allows, and `apply` then refused the table with `too many luma scaling points (15, limit 14)`. Room for the points that cut grain to zero at black or white was set aside from the measured levels before they were reduced to fit the limit. When the reduction dropped the darkest or brightest level and the constant level sat right next to it, one more point was added than was set aside. The points are now counted against the reduced levels, so a scene never has more than 14. Scenes that stayed within the limit give the same tables as 0.2.0.
- Reading a damaged table with a negative or very large point count no longer panics in debug builds. It is rejected as a table whose point count does not match its values, as release builds already did.

## 0.2.0 - 2026-10-15

### Features

- `generate` writes a table without measuring a video, with `--strength` for a grain deviation on the 0 to 255 scale or `--iso` for camera sensor noise after libaom's photon noise model. `--size fine|medium|coarse` sets how large the grain is and `--chroma` adds chroma grain. regrain builds the grain the way a decoder does, so the decoder output has the requested strength.
- `diff --just-enough` puts back only enough grain to hide banding: the measured grain, limited to a deviation of 0.5 on the 0 to 255 scale or the value given. On a dark gradient shown in 8 bit, it took the banding from 0.24 levels to 0.09, the same as the encode itself has in 10 bit.

### Fixes

- 0.1.3 cut the grain one level above the black or white of letterbox bars, to cover encoders that miss black by one level next to the picture. That level is also where pure black picture areas with grain sit, and on a letterboxed anime clip the error in the picture rose from 2.4% to 4.8% for almost no gain in the bars. The grain is now cut at the level of the bars only, which gives 3.0% in the picture and no grain in the bars. Picture areas at exactly that level still lose their grain, since AV1 sets grain by brightness alone.

### Tests

- `scripts/accuracy.py` has two more columns. `vs source` compares the noise in the grained decode with the noise in the source, so it also counts grain the encoder smoothed away, and a row for the encode without grain shows how much is missing to begin with. `spill` is the grain added where the source is perfectly constant, which the error never looked at.

## 0.1.3 - 2026-10-14

### Fixes

- Letterbox bars, black frames and other areas without grain in the source got grain in the encode. AV1 gives every brightness below the darkest measured level the grain of that level, so black bars at 16 got the grain of the darkest picture area, as strong as in the picture itself on a test with a grey picture between bars. `diff` now notes the levels where the source is perfectly constant and sets the grain to zero there when they lie outside the measured levels, one level further in to cover encoders that miss black by one level next to the picture. The same holds for constant white. Scenes without such areas give the same tables as 0.1.2.

## 0.1.2 - 2026-10-14

### Fixes

- `diff` compared the wrong frames when the source video starts later than its container, which is common in Blu-ray remuxes whose audio starts first. Every chunk after the first was read a few frames early in the source, so grain was measured on frames that did not match. Often this ended with `misaligned` partway through the video, even though both videos were in step. Seeks now count from the first frame of the video, so every chunk starts on the frame it should. Videos whose video track starts with the container give the same tables as 0.1.1.

## 0.1.1 - 2026-10-14

### Fixes

- `diff --no-chroma` measured luma grain too strong, by 20% to 35% on the test material, because ffmpeg stretched luma to full range while converting to gray. Videos are now decoded in their own format and regrain keeps only the luma, so `--no-chroma` measures the same luma as a run with chroma. Results are also the same whether the denoised video comes from a file or from standard input. Runs without `--no-chroma` give the same tables as 0.1.0.

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
