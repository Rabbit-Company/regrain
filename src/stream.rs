use crate::report::{Coded, fail};
use crate::table::{GrainTable, OPEN_END, Segment, TICKS_PER_SECOND};
use anyhow::{Context, Result};
use rabbit_av1::bits::{BitWriter, write_leb128};
use rabbit_av1::frame::NUM_REF_FRAMES;
use rabbit_av1::grain::{ChromaFormat, GrainParams};
use rabbit_av1::obu::OBU_FRAME;
use rabbit_av1::{CodedFrame, CodedGrain, ErrorKind, Input, ObuKind, Output, StreamParser, VideoReader};
use std::path::Path;

// A container that stores times in milliseconds can place a frame slightly
// before the segment boundary that belongs to it.
const TIMESTAMP_TOLERANCE: u64 = 10_000;

type FrameGrain = Option<(GrainParams, u16)>;

struct Scan {
  display: Vec<Option<u64>>,
  // Display time, the time the decoded frame was first shown, and its grain.
  shown: Vec<(u64, u64, FrameGrain)>,
  had_grain: bool,
  format: Option<ChromaFormat>,
}

fn scan(path: &Path, ffmpeg: &str, fps: Option<(u32, u32)>) -> Result<Scan> {
  let mut input = Input::open(path, ffmpeg, fps)?;
  let mut result = match scan_reader(&mut input.reader) {
    Ok(result) => result,
    Err(error) if crate::cancel::requested() => return Err(error),
    Err(error) => return Err(explain(&mut input, error)),
  };
  input.finish()?;
  if result.display.is_empty() {
    return Err(fail("unsupported_input", format!("no AV1 frames found in {}", path.display())));
  }
  result.display.shrink_to_fit();
  Ok(result)
}

fn scan_reader(reader: &mut VideoReader) -> Result<Scan> {
  let mut parser = StreamParser::new();
  let mut result = Scan {
    display: Vec::new(),
    shown: Vec::new(),
    had_grain: false,
    format: None,
  };
  let mut slot_frame: [Option<usize>; NUM_REF_FRAMES] = [None; NUM_REF_FRAMES];
  let mut slot_grain: [Option<(GrainParams, u16)>; NUM_REF_FRAMES] = Default::default();
  let mut packets = 0u64;
  while let Some(packet) = reader.next_packet()? {
    crate::cancel::check()?;
    packets += 1;
    let ticks = reader.timestamp(packet.pts, TICKS_PER_SECOND);
    let obus = parser
      .parse_packet(&packet.data)
      .with_context(|| format!("packet {packets}"))
      .code("invalid_bitstream")?;
    for obu in obus {
      let ObuKind::Frame(frame) = obu.kind else {
        if let ObuKind::Sequence(sequence) = &obu.kind {
          result.had_grain |= sequence.film_grain_params_present;
        }
        continue;
      };
      let header = &frame.header;
      if header.show_existing_frame {
        let slot = header.frame_to_show;
        let first = match slot_frame[slot] {
          Some(index) => *result.display[index].get_or_insert(ticks),
          None => ticks,
        };
        result.shown.push((ticks, first, slot_grain[slot].clone()));
        if header.is_key() {
          slot_frame = [slot_frame[slot]; NUM_REF_FRAMES];
          let grain = slot_grain[slot].clone();
          slot_grain = std::array::from_fn(|_| grain.clone());
        }
        continue;
      }
      let index = result.display.len();
      result.display.push(header.show_frame.then_some(ticks));
      let grain = match &frame.grain {
        CodedGrain::Full { seed, params } => Some((params.clone(), *seed)),
        CodedGrain::Reference { seed, slot } => slot_grain[*slot].as_ref().map(|(params, _)| (params.clone(), *seed)),
        CodedGrain::Absent | CodedGrain::NotApplied => None,
      };
      if header.show_frame {
        result.shown.push((ticks, ticks, grain.clone()));
      }
      for slot in 0..NUM_REF_FRAMES {
        if header.refresh_frame_flags >> slot & 1 == 1 {
          slot_frame[slot] = Some(index);
          slot_grain[slot] = grain.clone();
        }
      }
    }
  }
  result.format = parser.format();
  Ok(result)
}

pub fn export(path: &Path, ffmpeg: &str, fps: Option<(u32, u32)>) -> Result<(GrainTable, usize)> {
  let mut scan = scan(path, ffmpeg, fps)?;
  let shown = scan.shown.len();
  scan.shown.sort_by_key(|(ticks, _, _)| *ticks);
  let mut table = GrainTable::default();
  let mut previous: Option<GrainParams> = None;
  for (ticks, _, grain) in scan.shown {
    let params = grain.as_ref().map(|(params, _)| params);
    if params == previous.as_ref() {
      continue;
    }
    if previous.is_some()
      && let Some(last) = table.segments.last_mut()
    {
      last.end = ticks;
    }
    // Frames that share a timestamp would leave an entry without length behind.
    if table.segments.last().is_some_and(|last| last.end <= last.start) {
      table.segments.pop();
    }
    if let Some((params, seed)) = &grain {
      table.segments.push(Segment {
        start: ticks,
        end: OPEN_END,
        seed: *seed,
        params: params.clone(),
      });
    }
    previous = params.cloned();
  }
  Ok((table, shown))
}

pub struct ApplyOptions<'a> {
  pub ffmpeg: &'a str,
  pub fps: Option<(u32, u32)>,
  pub compact: bool,
  pub static_grain: bool,
  pub verify: bool,
}

pub struct ApplyReport {
  pub frames: usize,
  pub grained: usize,
  pub replaced: bool,
  pub empty_table: bool,
  pub verified: bool,
}

fn segment_at(table: &GrainTable, ticks: u64) -> Option<usize> {
  table.find(ticks + TIMESTAMP_TOLERANCE).or_else(|| table.find(ticks))
}

// When reading fails while ffmpeg feeds the input, ffmpeg usually stopped first.
fn explain(input: &mut Input, error: anyhow::Error) -> anyhow::Error {
  let status = input.explain(rabbit_av1::Error::new(ErrorKind::Io, "reading stopped"));
  if status.kind() == ErrorKind::FfmpegFailed {
    status.into()
  } else {
    error
  }
}

// Decoder messages without the addresses ffmpeg puts in front of them.
fn decode_messages(path: &Path, ffmpeg: &str) -> Result<(bool, Vec<String>)> {
  rabbit_av1::container::decode_messages(path, ffmpeg).map_err(|error| {
    if error.kind() == ErrorKind::DecoderMissing {
      fail(
        "verify_unavailable",
        format!("{ffmpeg} has no libdav1d decoder, which --verify needs"),
      )
    } else {
      error.into()
    }
  })
}

// Reads the written stream back and checks that every shown frame carries the
// grain it should, then lets a real decoder go through all of it.
fn verify(path: &Path, input: &Path, table: Option<&GrainTable>, ffmpeg: &str, fps: (u32, u32), frames_shown: usize) -> Result<()> {
  let written = scan(path, ffmpeg, Some(fps)).code("verify_failed")?;
  let format = written.format.context("the output has no sequence header").code("verify_failed")?;
  if written.shown.len() != frames_shown {
    return Err(fail(
      "verify_failed",
      format!("the output shows {} frames, the input showed {frames_shown}", written.shown.len()),
    ));
  }
  // A frame shown twice keeps the grain of its first showing.
  for (_, ticks, grain) in &written.shown {
    let expected = table
      .and_then(|table| segment_at(table, *ticks).map(|index| table.segments[index].params.normalized(format)))
      .filter(|params| !params.is_empty());
    if let Some((params, _)) = grain {
      let errors = params.conformance_errors(format);
      if !errors.is_empty() {
        return Err(fail(
          "verify_failed",
          format!("the output breaks an AV1 film grain rule: {}", errors.join(", ")),
        ));
      }
    }
    let actual = grain.as_ref().map(|(params, _)| params.normalized(format));
    if actual != expected {
      let seconds = *ticks as f64 / TICKS_PER_SECOND as f64;
      return Err(fail(
        "verify_failed",
        format!("the frame at {seconds:.3} s does not carry the grain from the table"),
      ));
    }
  }
  let (decoded, messages) = decode_messages(path, ffmpeg)?;
  if decoded && messages.is_empty() {
    return Ok(());
  }
  // Damaged or unusual input can make the decoder complain about the original too.
  // Only complaints that the original does not cause count.
  let (input_decoded, known) = decode_messages(input, ffmpeg)?;
  let new: Vec<&String> = messages.iter().filter(|message| !known.contains(message)).collect();
  if (!decoded && input_decoded) || !new.is_empty() {
    let first = new.first().map(|message| message.as_str()).unwrap_or("the decoder failed");
    return Err(fail("verify_failed", format!("dav1d could not decode the output: {first}")));
  }
  Ok(())
}

// Every frame needs its own seed or the grain pattern stands still, unless a
// still pattern is what was asked for. The first frame of a segment keeps the
// table's seed, so an exported table matches.
fn frame_seed(segment: &Segment, ticks: u64, static_grain: bool) -> u16 {
  if static_grain {
    return segment.seed;
  }
  let milliseconds = (ticks.saturating_sub(segment.start) + 5_000) / 10_000;
  (segment.seed as u64).wrapping_add(milliseconds.wrapping_mul(40503)) as u16
}

struct Rewriter<'a> {
  table: Option<&'a GrainTable>,
  display: Vec<Option<u64>>,
  compact: bool,
  static_grain: bool,
  normalized: Vec<Option<GrainParams>>,
  format: Option<ChromaFormat>,
  slot_segment: [Option<usize>; NUM_REF_FRAMES],
  index: usize,
  grained: usize,
  last_header: Option<Vec<u8>>,
}

impl<'a> Rewriter<'a> {
  fn new(table: Option<&'a GrainTable>, display: Vec<Option<u64>>, compact: bool, static_grain: bool) -> Self {
    Rewriter {
      table,
      display,
      compact,
      static_grain,
      normalized: Vec::new(),
      format: None,
      slot_segment: [None; NUM_REF_FRAMES],
      index: 0,
      grained: 0,
      last_header: None,
    }
  }

  fn segment_for(&mut self, format: ChromaFormat) -> Option<usize> {
    let table = self.table?;
    if self.format != Some(format) {
      self.format = Some(format);
      self.normalized = table
        .segments
        .iter()
        .map(|segment| {
          let params = segment.params.normalized(format);
          (!params.is_empty()).then_some(params)
        })
        .collect();
    }
    let ticks = (*self.display.get(self.index)?)?;
    let found = segment_at(table, ticks)?;
    self.normalized[found].as_ref().map(|_| found)
  }

  fn frame_payload(&mut self, payload: &[u8], frame: &CodedFrame, format: ChromaFormat, is_frame_obu: bool) -> Vec<u8> {
    let header = &frame.header;
    let mut w = BitWriter::new();
    w.copy_bits(payload, 0, header.grain_bit);
    let mut stored = None;
    if header.grain_allowed() && self.table.is_some() {
      match self.segment_for(format) {
        None => w.bit(false),
        Some(segment) => {
          let table = self.table.unwrap();
          w.bit(true);
          let ticks = self.display.get(self.index).copied().flatten().unwrap_or(0);
          w.bits(frame_seed(&table.segments[segment], ticks, self.static_grain) as u32, 16);
          let reference = if self.compact && header.is_inter() {
            header
              .ref_frame_idx
              .iter()
              .copied()
              .find(|&slot| self.slot_segment[slot] == Some(segment))
          } else {
            None
          };
          if header.is_inter() {
            w.bit(reference.is_none());
          }
          match reference {
            Some(slot) => w.bits(slot as u32, 3),
            None => self.normalized[segment].as_ref().unwrap().write(&mut w, format),
          }
          stored = Some(segment);
          self.grained += 1;
        }
      }
    }
    for slot in 0..NUM_REF_FRAMES {
      if header.refresh_frame_flags >> slot & 1 == 1 {
        self.slot_segment[slot] = stored;
      }
    }
    self.index += 1;
    if is_frame_obu {
      w.align();
      w.extend(&payload[frame.header_end_bit.div_ceil(8)..]);
    } else {
      w.trailing_bits();
    }
    w.finish()
  }

  fn packet(&mut self, parser: &mut StreamParser, data: &[u8]) -> Result<Vec<u8>> {
    let obus = parser.parse_packet(data)?;
    let mut out = Vec::with_capacity(data.len() + 256);
    for obu in &obus {
      let payload = &data[obu.payload.clone()];
      let replacement = match &obu.kind {
        ObuKind::Sequence(sequence) => {
          let mut bytes = payload.to_vec();
          let mask = 1 << (7 - (sequence.film_grain_bit & 7));
          if self.table.is_some() {
            bytes[sequence.film_grain_bit >> 3] |= mask;
          } else {
            bytes[sequence.film_grain_bit >> 3] &= !mask;
          }
          self.last_header = None;
          Some(bytes)
        }
        ObuKind::Frame(frame) if frame.header.show_existing_frame => {
          if frame.header.is_key() {
            self.slot_segment = [self.slot_segment[frame.header.frame_to_show]; NUM_REF_FRAMES];
          }
          self.last_header = None;
          None
        }
        ObuKind::Frame(frame) => {
          let format = parser.format().context("frame found before a sequence header")?;
          let is_frame_obu = obu.obu_type == OBU_FRAME;
          let bytes = self.frame_payload(payload, frame, format, is_frame_obu);
          self.last_header = (!is_frame_obu).then(|| bytes.clone());
          Some(bytes)
        }
        ObuKind::FrameCopy => self.last_header.clone(),
        _ => None,
      };
      out.extend_from_slice(&data[obu.prefix.clone()]);
      let bytes = replacement.as_deref().unwrap_or(payload);
      if obu.has_size {
        write_leb128(&mut out, bytes.len() as u64);
      }
      out.extend_from_slice(bytes);
    }
    Ok(out)
  }
}

pub fn apply(input_path: &Path, output_path: &Path, table: Option<&GrainTable>, options: &ApplyOptions) -> Result<ApplyReport> {
  let empty_table = table.is_some_and(|table| table.segments.is_empty());
  let table = table.filter(|table| !table.segments.is_empty());
  let scan = scan(input_path, options.ffmpeg, options.fps)?;
  let frames_shown = scan.shown.len();
  let frames = scan.display.len();
  let mut rewriter = Rewriter::new(table, scan.display.clone(), options.compact, options.static_grain);
  let mut input = Input::open(input_path, options.ffmpeg, options.fps)?;
  let fps = (input.reader.rate, input.reader.scale);
  let mut output = Output::create(output_path, input_path, &input.reader, options.ffmpeg)?;
  let mut parser = StreamParser::new();
  let result = (|| -> Result<()> {
    while let Some(packet) = input.reader.next_packet()? {
      crate::cancel::check()?;
      let data = rewriter.packet(&mut parser, &packet.data).code("invalid_bitstream")?;
      output.write_packet(packet.pts, &data, packet.units.as_deref())?;
    }
    Ok(())
  })();
  if let Err(error) = result {
    output.discard();
    return Err(if crate::cancel::requested() {
      error
    } else {
      explain(&mut input, error)
    });
  }
  // A cancelled run stops before ffmpeg muxes the container.
  if let Err(error) = input.finish().map_err(anyhow::Error::from).and_then(|()| crate::cancel::check()) {
    output.discard();
    return Err(error);
  }
  let written = output.finish()?;
  if let Err(error) = crate::cancel::check() {
    written.discard();
    return Err(error);
  }
  if options.verify
    && let Err(error) = verify(&written.temp, input_path, table, options.ffmpeg, fps, frames_shown)
  {
    written.discard();
    return Err(error);
  }
  if let Err(error) = crate::cancel::check() {
    written.discard();
    return Err(error);
  }
  written.commit()?;
  Ok(ApplyReport {
    frames,
    grained: rewriter.grained,
    replaced: scan.had_grain,
    empty_table,
    verified: options.verify,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;
  use std::panic::{AssertUnwindSafe, catch_unwind};

  const TABLE: &str = "filmgrn1\nE 0 2000000 1 100 1\n  p 2 7 0 10 0 1 128 192 256 128 192 256\n  sY 3 0 20 100 60 255 25\n  sCb 2 0 20 255 30\n  sCr 1 128 25\n  cY 2 4 6 4 2 4 10 24 10 4 6 26\n  cCb 0 0 4 0 0 0 6 16 6 0 4 18 20\n  cCr 0 0 0 0 0 0 0 0 0 0 0 0 0\nE 2000000 9223372036854775807 1 200 1\n  p 0 6 0 11 0 1 128 192 256 128 192 256\n  sY 1 128 40\n  sCb 0\n  sCr 0\n  cY\n  cCb 0\n  cCr 0\n";

  // Runs a stream through everything apply and export do. Errors are fine, panics are not.
  fn exercise(data: &[u8], table: &GrainTable) {
    let Ok(mut reader) = VideoReader::open(Box::new(Cursor::new(data.to_vec())), None) else {
      return;
    };
    let Ok(scan) = scan_reader(&mut reader) else { return };
    let Ok(mut reader) = VideoReader::open(Box::new(Cursor::new(data.to_vec())), None) else {
      return;
    };
    let mut rewriter = Rewriter::new(Some(table), scan.display, true, false);
    let mut parser = StreamParser::new();
    while let Ok(Some(packet)) = reader.next_packet() {
      if rewriter.packet(&mut parser, &packet.data).is_err() {
        break;
      }
    }
  }

  #[test]
  fn static_grain_keeps_the_entry_seed() {
    let table = GrainTable::parse(TABLE).unwrap();
    let segment = &table.segments[0];
    assert_eq!(frame_seed(segment, 0, false), 100);
    assert_ne!(frame_seed(segment, 417_083, false), 100);
    assert_eq!(frame_seed(segment, 417_083, true), 100);
  }

  struct Rng(u64);

  impl Rng {
    fn below(&mut self, limit: usize) -> usize {
      self.0 ^= self.0 << 13;
      self.0 ^= self.0 >> 7;
      self.0 ^= self.0 << 17;
      (self.0 % limit.max(1) as u64) as usize
    }
  }

  fn mutate(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = data.to_vec();
    // The 32 byte IVF header stays mostly intact so that mutations reach the AV1 data.
    let body = 32.min(out.len());
    for _ in 0..1 + rng.below(4) {
      if out.len() <= body {
        break;
      }
      let at = body + rng.below(out.len() - body);
      match rng.below(5) {
        0 => out[at] ^= 1 << rng.below(8),
        1 => out[at] = rng.below(256) as u8,
        2 => out.truncate(at),
        3 => {
          let end = (at + 1 + rng.below(64)).min(out.len());
          let copy = out[at..end].to_vec();
          let to = body + rng.below(out.len() - body);
          out.splice(to..to, copy);
        }
        _ => {
          let end = (at + 1 + rng.below(16)).min(out.len());
          out.drain(at..end);
        }
      }
    }
    out
  }

  fn sample_streams() -> Vec<(String, Vec<u8>)> {
    if let Some(folder) = std::env::var_os("REGRAIN_FUZZ_DIR") {
      let mut streams: Vec<(String, Vec<u8>)> = std::fs::read_dir(folder)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "ivf" || ext == "obu"))
        .map(|path| (path.display().to_string(), std::fs::read(&path).unwrap()))
        .collect();
      streams.sort();
      return streams;
    }
    let path = std::env::temp_dir().join(format!("regrain-fuzz-{}.ivf", std::process::id()));
    let made = std::process::Command::new("ffmpeg")
      .args([
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=96x64:rate=24",
        "-frames:v",
        "20",
      ])
      .args(["-pix_fmt", "yuv420p10le", "-c:v", "libsvtav1", "-preset", "12"])
      .arg(&path)
      .status()
      .is_ok_and(|status| status.success());
    let data = std::fs::read(&path).ok().filter(|_| made);
    let _ = std::fs::remove_file(&path);
    data.map(|data| vec![("generated".to_string(), data)]).unwrap_or_default()
  }

  #[test]
  fn damaged_streams_never_panic() {
    let table = GrainTable::parse(TABLE).unwrap();
    let streams = sample_streams();
    if streams.is_empty() {
      eprintln!("skipped: no sample stream, set REGRAIN_FUZZ_DIR or install ffmpeg with libsvtav1");
      return;
    }
    let rounds: usize = std::env::var("REGRAIN_FUZZ_ROUNDS")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(3000 / streams.len())
      .max(1);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for (name, data) in &streams {
      exercise(data, &table);
      for round in 0..rounds {
        let damaged = mutate(data, &mut rng);
        if catch_unwind(AssertUnwindSafe(|| exercise(&damaged, &table))).is_err() {
          let path = std::env::temp_dir().join("regrain-fuzz-crash.ivf");
          std::fs::write(&path, &damaged).unwrap();
          panic!("{name} round {round} panicked, the damaged stream is in {}", path.display());
        }
      }
    }
  }

  #[test]
  fn damaged_tables_never_panic() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let alphabet = b" \t\n-0123456789EpsYCbcr";
    for _ in 0..20_000 {
      let mut text = TABLE.as_bytes().to_vec();
      for _ in 0..1 + rng.below(6) {
        let at = rng.below(text.len());
        match rng.below(3) {
          0 => text[at] = alphabet[rng.below(alphabet.len())],
          1 => {
            text.remove(at);
          }
          _ => text.insert(at, alphabet[rng.below(alphabet.len())]),
        }
      }
      let _ = GrainTable::parse(&String::from_utf8_lossy(&text));
    }
  }
}
