# The `filmgrn1` film grain table format

This document describes the `filmgrn1` text format for AV1 film grain tables as regrain reads and writes it. The layout follows the FGS table convention by PingWer74, the author of FGSEditor. What the values mean follows libaom, which defines the format in [`aom_dsp/grain_table.c`](https://aomedia.googlesource.com/aom/+/refs/heads/main/aom_dsp/grain_table.c) and is the reference for every other tool. Section 6 lists what libaom, SVT-AV1 and grav1synth actually do with a table, as measured.

The parameters in a table are the film grain parameters of AV1 ([film grain params semantics](https://aomediacodec.github.io/av1-spec/#film-grain-params-semantics)). AFGS1, the AOMedia film grain metadata for other codecs, uses the same grain model but can also carry things a `filmgrn1` table cannot, such as several parameter sets for different resolutions.

The words MUST, SHOULD and MAY are used as in RFC 2119. A _writer_ is any program that creates a table, a _parser_ any program that reads one.

## 1. File

- The file is UTF-8 text, in practice plain ASCII. Writers MUST NOT add a byte order mark; parsers SHOULD skip one.
- Writers MUST end lines with a line feed (`\n`). Parsers SHOULD also accept a carriage return before it, and blank lines anywhere.
- The `filmgrn1` line appears exactly once. Parsers MUST reject a second one, and MUST reject other versions such as `filmgrn2` instead of reading them with this grammar.
- The convention allows comments before the `filmgrn1` line, and parsers SHOULD skip them. Writers SHOULD NOT write any, and SHOULD start the file with `filmgrn1`: libaom encodes without grain when anything comes before it, and SVT-AV1 and grav1synth refuse the table.
- There are no comments after the `filmgrn1` line.
- `.tbl` and `.txt` files hold the same format. `.tbl` is the usual name for encoders, `.txt` for opening tables in a text editor.

## 2. Entries

After the `filmgrn1` line, the file is a list of entries. Each entry starts with an `E` line, followed by its parameter lines when it has its own parameters.

### 2.1 The `E` line

```text
E <start> <end> <apply_grain> <random_seed> <update_parameters>
```

| Field               | Values           | Meaning                                                        |
| ------------------- | ---------------- | -------------------------------------------------------------- |
| `start`             | 0 to 2⁶³−1       | Start time in units of 1/10 000 000 second                     |
| `end`               | `start` to 2⁶³−1 | End time in the same units                                     |
| `apply_grain`       | 0 or 1           | 1 adds grain during the entry, 0 marks a stretch without grain |
| `random_seed`       | 0 to 65535       | Seed of the decoder's random numbers for the grain pattern     |
| `update_parameters` | 1                | 1 when parameter lines follow. Writers MUST write 1, see below |

**Time units.** One second is `10000000`. libaom converts every frame's timestamp to these units (`TICKS_PER_SEC` in `av1/encoder/encoder.h`) and compares them with the table's times, and grav1synth, regrain and FGSEditor use the same units. A table for 24000/1001 fps video places frame 1 at `417083`. They are not nanoseconds: a table in nanoseconds would place every change 100 times too late.

**Which frames an entry covers.** A frame shown at time `t` belongs to the entry with `start <= t < end`, as in libaom's lookup. The start is included and the end is not.

**Order.** Entries MUST be in order of time and MUST NOT overlap: for consecutive entries, `start` of the later one is at least `end` of the earlier one.

**Gaps.** Time between two entries is allowed. Frames there get no grain. A writer that leaves a gap on purpose SHOULD fill it with an entry with `apply_grain` 0, so that tools can tell an intended gap from a mistake.

**Length.** Writers MUST write `end` greater than `start`. An entry with `end` equal to `start` covers no frame. Parsers MUST accept it and ignore it, since tables in use contain such entries.

**Open end.** `end` MAY be `9223372036854775807` (INT64_MAX) to mean that the entry lasts until the end of the video. libaom reads the times as signed 64-bit numbers, so larger values such as UINT64_MAX are invalid.

**Seed.** Writers SHOULD NOT use seed 0 for an entry with luma grain. The decoder's random number register then stays at zero, and luma gets a constant offset instead of grain. Encoders MAY derive a different seed for every frame from the entry's seed, so that the grain pattern moves.

**`update_parameters` 0.** An entry with 0 has no parameter lines at all, and the next line is the next `E` line. The convention gives such an entry the parameters of the entry before it, and regrain reads it that way, but libaom gives it no grain. Writers MUST write 1 and give every entry its parameter lines, so the table means the same everywhere. Parsers MUST accept 0 and SHOULD warn about it.

### 2.2 Parameter lines

An entry with `update_parameters` 1 has exactly these seven lines, in this order, each indented by two spaces:

```text
  p <ar_coeff_lag> <ar_coeff_shift> <grain_scale_shift> <scaling_shift> <chroma_scaling_from_luma> <overlap_flag> <cb_mult> <cb_luma_mult> <cb_offset> <cr_mult> <cr_luma_mult> <cr_offset>
  sY <n> <x> <y> ...
  sCb <n> <x> <y> ...
  sCr <n> <x> <y> ...
  cY <coefficients>
  cCb <coefficients>
  cCr <coefficients>
```

Values are separated by single spaces. Parsers read the lines in this order and SHOULD accept any whitespace at the start of a line and between values, which includes libaom's own layout with a tab before each line.

#### `p`

Twelve integers:

| Field                      | Values   | Meaning                                                                    |
| -------------------------- | -------- | -------------------------------------------------------------------------- |
| `ar_coeff_lag`             | 0 to 3   | How far the autoregressive filter reaches; sets the number of coefficients |
| `ar_coeff_shift`           | 6 to 9   | The filter coefficients are divided by 2 to the power of this              |
| `grain_scale_shift`        | 0 to 3   | Makes the Gaussian noise weaker by 2 to the power of this                  |
| `scaling_shift`            | 8 to 11  | The scaling values are divided by 2 to the power of this                   |
| `chroma_scaling_from_luma` | 0 or 1   | 1 makes Cb and Cr use the luma scaling function                            |
| `overlap_flag`             | 0 or 1   | 1 blends the edges between blocks of grain                                 |
| `cb_mult`                  | 0 to 255 | Weight of the Cb value in the Cb scaling index, 128 is none                |
| `cb_luma_mult`             | 0 to 255 | Weight of the luma value in the Cb scaling index, 128 is none              |
| `cb_offset`                | 0 to 511 | Offset of the Cb scaling index, 256 is none                                |
| `cr_mult`                  | 0 to 255 | As `cb_mult`, for Cr                                                       |
| `cr_luma_mult`             | 0 to 255 | As `cb_luma_mult`, for Cr                                                  |
| `cr_offset`                | 0 to 511 | As `cb_offset`, for Cr                                                     |

The decoder looks up the chroma scaling at index `((luma × (luma_mult − 128) + chroma × (mult − 128)) >> 6) + offset − 256`, clipped to the sample range. With `mult` 128, `luma_mult` 192 and `offset` 256, the index is the luma value, so the chroma scaling function follows brightness like the luma one. Writers SHOULD use these values when they have no reason for others; libaom, SVT-AV1 and regrain use them too. With `luma_mult` 128 as well, the index is 0 for every pixel, and chroma grain takes the value at brightness 0 everywhere.

The format has no field for AV1's `clip_to_restricted_range`. The tool that writes the parameters into a video decides it.

#### `sY`, `sCb`, `sCr`

The scaling function of each plane: how strong the grain is at each brightness, as a number of points followed by that many `x y` pairs.

- `x` is the brightness on a 0 to 255 scale, `y` the scaling value from 0 to 255.
- `x` MUST strictly increase from one point to the next.
- `sY` has at most 14 points, `sCb` and `sCr` at most 10 each.
- A plane without grain has 0 points.
- When `chroma_scaling_from_luma` is 1, `sCb` and `sCr` MUST have 0 points.
- Between points the decoder interpolates in a straight line. Below the first point and above the last, the value of that point holds.
- The grain's strength is the scaling value divided by `2^scaling_shift`, times the strength of the grain pattern. With `scaling_shift` 11 and the filter of the examples below, a value of 50 gives a deviation of about 0.8 on the 0 to 255 scale. `regrain stats` shows the deviation for any table.

```text
  sY 2 16 50 235 50
  sCb 0
```

libaom writes two spaces after the point count of `sY` and ends `sY 0` with a space. Parsers read that the same way.

#### `cY`, `cCb`, `cCr`

The coefficients of the autoregressive filter, each from −128 to 127. With `L` the `ar_coeff_lag`, `cY` has `2 × L × (L + 1)` values and `cCb` and `cCr` have one more, which is the weight of the luma grain in that chroma plane:

| `ar_coeff_lag` | `cY` | `cCb`, `cCr` |
| -------------- | ---- | ------------ |
| 0              | 0    | 1            |
| 1              | 4    | 5            |
| 2              | 12   | 13           |
| 3              | 24   | 25           |

All three lines always hold their full count, also for a plane without grain. Coefficients of a plane without grain have no effect; writers SHOULD write them as 0, so that equal grain gives equal tables.

At lag 0, `cY` is a line without values, and `cCb` and `cCr` hold only the luma weight. It is usually 0 but may be any value from −128 to 127.

## 3. Rules from AV1

Some parameters are valid in a table but change when the table is written into a video, because AV1 cannot carry them. Writers SHOULD avoid them, and tools that check tables SHOULD warn about them. regrain's `check` command reports each one.

- In 4:2:0 video, chroma grain needs luma grain. Without luma points, `sCb` and `sCr` are dropped.
- In 4:2:0 video, Cb and Cr have grain together or not at all. If only one has points, the other gets a single point at zero strength.
- In monochrome video, only luma grain is used.
- The autoregressive filter SHOULD be stable. When its coefficients add up to the divisor `2^ar_coeff_shift` or more, the grain can grow across the decoder's grain template, and blocks of grain then differ in strength.

## 4. Writers and parsers

**Writers** produce:

- `filmgrn1` and a line feed as the first line, with nothing before it.
- The `E` line with single spaces and no indentation.
- The parameter lines indented by two spaces, with single spaces between values.
- A line feed after every line, including the last.

The same entries are then written as the same bytes by every tool that follows this document, and libaom, SVT-AV1 and grav1synth read the table as written.

**Parsers** MUST accept every table libaom and the convention's writers produce: tabs or spaces before lines, any whitespace between values, carriage returns, blank lines, a byte order mark and comments before `filmgrn1`. They MUST reject a table that breaks a MUST of this document, with a message that names the line.

## 5. Examples

One entry for a whole video, luma grain only:

```text
filmgrn1
E 0 9223372036854775807 1 7391 1
  p 3 8 0 11 0 1 128 192 256 128 192 256
  sY 2 16 50 235 50
  sCb 0
  sCr 0
  cY 7 -2 -15 -19 -9 -2 1 -5 -15 -15 -30 -10 -16 -4 -14 -11 -16 5 -14 -10 -18 -16 -24 10
  cCb 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
  cCr 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
```

Four entries: grain with chroma from 0 to 4 seconds, the same grain with a new seed from 4 to 6 seconds, no grain from 6 to 8 seconds, and lighter grain from 8 seconds to the end:

```text
filmgrn1
E 0 40000000 1 7391 1
  p 3 8 0 11 0 1 128 192 256 128 192 256
  sY 2 16 40 235 40
  sCb 2 16 20 235 20
  sCr 2 16 20 235 20
  cY 7 -2 -15 -19 -9 -2 1 -5 -15 -15 -30 -10 -16 -4 -14 -11 -16 5 -14 -10 -18 -16 -24 10
  cCb -10 -24 -35 -15 -28 -9 -6 -13 -42 -51 -95 -23 -18 -7 -48 -45 -117 -53 -95 -9 16 1 -76 -40 21
  cCr -8 -23 -28 -24 -17 -22 2 -5 -32 -58 -80 -45 -13 -1 -35 -45 -104 -50 -77 -12 -14 6 -73 -46 14
E 40000000 60000000 1 2415 1
  p 3 8 0 11 0 1 128 192 256 128 192 256
  sY 2 16 40 235 40
  sCb 2 16 20 235 20
  sCr 2 16 20 235 20
  cY 7 -2 -15 -19 -9 -2 1 -5 -15 -15 -30 -10 -16 -4 -14 -11 -16 5 -14 -10 -18 -16 -24 10
  cCb -10 -24 -35 -15 -28 -9 -6 -13 -42 -51 -95 -23 -18 -7 -48 -45 -117 -53 -95 -9 16 1 -76 -40 21
  cCr -8 -23 -28 -24 -17 -22 2 -5 -32 -58 -80 -45 -13 -1 -35 -45 -104 -50 -77 -12 -14 6 -73 -46 14
E 60000000 80000000 0 1 1
  p 0 6 0 8 0 1 128 192 256 128 192 256
  sY 0
  sCb 0
  sCr 0
  cY
  cCb 0
  cCr 0
E 80000000 9223372036854775807 1 1234 1
  p 3 8 0 11 0 1 128 192 256 128 192 256
  sY 2 16 25 235 25
  sCb 0
  sCr 0
  cY 7 -2 -15 -19 -9 -2 1 -5 -15 -15 -30 -10 -16 -4 -14 -11 -16 5 -14 -10 -18 -16 -24 10
  cCb 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
  cCr 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
```

The second entry repeats the parameters of the first in full with its own seed, since `update_parameters` 0 is not portable. The third has `apply_grain` 0 and marks 6 to 8 seconds as meant to have no grain. It still has parameter lines, because libaom reads them whenever `update_parameters` is 1.

## 6. What tools do with a table

Measured in October 2026 with libaom at commit `8f126544` through ffmpeg 8.1, SVT-AV1 v3.1.2's `SvtAv1EncApp` and grav1synth 0.3.8, by reading every decoded frame's grain parameters back with ffprobe and dav1d:

|                            | libaom                                                        | SVT-AV1                                      | grav1synth `apply`                                                      | regrain `apply`                                                                                        |
| -------------------------- | ------------------------------------------------------------- | -------------------------------------------- | ----------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| Times                      | Every entry, in 1/10 000 000 s                                | Ignored: the first entry goes on every frame | Every entry, but frames near a boundary can take the neighbouring entry | Every entry, in 1/10 000 000 s                                                                         |
| Seed                       | The entry's seed on its first frame, then a new one per frame | Ignored                                      | Not measured                                                            | The entry's seed on its first frame, then a new one per frame, or on every frame with `--static-grain` |
| Two-space or tab layout    | Both                                                          | Both                                         | Both                                                                    | Both                                                                                                   |
| Comments before `filmgrn1` | Encodes without grain, without an error                       | Refuses the table                            | Refuses the table                                                       | Skips them                                                                                             |
| `update_parameters` 0      | No grain for the entry                                        | Only the first entry is read                 | Not measured                                                            | Parameters of the entry before                                                                         |

SVT-AV1 reads the table in its command line app, so ffmpeg's `libsvtav1` cannot pass one. Its reader says "TODO Add functionality to read multiple grain table entries", in SVT-AV1 master as of October 2026 too.

## 7. References

- libaom's reader and writer: [`aom_dsp/grain_table.c`](https://aomedia.googlesource.com/aom/+/refs/heads/main/aom_dsp/grain_table.c)
- libaom's time units: [`av1/encoder/encoder.h`](https://aomedia.googlesource.com/aom/+/8f126544519961bb3d5afe83455f4caf5d312d05/av1/encoder/encoder.h#4067) and the table lookup in [`av1/encoder/encode_strategy.c`](https://aomedia.googlesource.com/aom/+/8f126544519961bb3d5afe83455f4caf5d312d05/av1/encoder/encode_strategy.c#1507)
- AV1 film grain parameters: [syntax](https://aomediacodec.github.io/av1-spec/#film-grain-params-syntax) and [semantics](https://aomediacodec.github.io/av1-spec/#film-grain-params-semantics)
- How a decoder synthesizes the grain: [film grain synthesis process](https://aomediacodec.github.io/av1-spec/#film-grain-synthesis-process)
- AFGS1, the same grain model as metadata for other codecs: [AFGS1 specification](https://aomediacodec.github.io/afgs1-spec/)
