use crate::av1::bits::{read_leb128, write_leb128};
use crate::av1::obu::OBU_TEMPORAL_DELIMITER;
use crate::av1::sequence::SequenceHeader;
use crate::ivf::{HEADER_SIZE, IvfReader, Packet};
use crate::report::{Coded, fail};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};

const MAX_UNIT: u64 = 1 << 28;
pub const DEFAULT_FPS: (u32, u32) = (24000, 1001);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
  Ivf,
  LowOverhead,
  AnnexB,
}

trait PacketSource {
  fn next_packet(&mut self) -> Result<Option<Packet>>;
}

impl<R: Read> PacketSource for IvfReader<R> {
  fn next_packet(&mut self) -> Result<Option<Packet>> {
    IvfReader::next_packet(self)
  }
}

// Reads a leb128 value from a stream. None means the stream ended before it began.
fn read_leb128_from(input: &mut impl Read) -> Result<Option<u64>> {
  let mut value = 0u64;
  for i in 0..8 {
    let mut byte = [0u8];
    if input.read(&mut byte).code("io_error")? == 0 {
      if i == 0 {
        return Ok(None);
      }
      return Err(fail("invalid_bitstream", "the stream ends inside a size field"));
    }
    value |= ((byte[0] & 0x7f) as u64) << (i * 7);
    if byte[0] & 0x80 == 0 {
      return Ok(Some(value));
    }
  }
  Err(fail("invalid_bitstream", "invalid size field"))
}

fn read_exact_unit(input: &mut impl Read, size: u64) -> Result<Vec<u8>> {
  if size > MAX_UNIT {
    return Err(fail(
      "invalid_bitstream",
      format!("a unit claims {size} bytes, the stream is damaged"),
    ));
  }
  let mut data = vec![0u8; size as usize];
  input
    .read_exact(&mut data)
    .context("the stream ends inside a unit")
    .code("invalid_bitstream")?;
  Ok(data)
}

// Returns the byte length of the OBU header, including the extension byte.
fn header_length(first: u8) -> usize {
  if first & 0x04 != 0 { 2 } else { 1 }
}

// A Section 5 stream: OBUs with size fields, where a temporal delimiter starts every
// temporal unit.
struct LowOverheadSource<R: Read> {
  input: R,
  carry: Option<Vec<u8>>,
  index: u64,
}

impl<R: Read> LowOverheadSource<R> {
  fn next_obu(&mut self) -> Result<Option<Vec<u8>>> {
    let mut first = [0u8];
    if self.input.read(&mut first).code("io_error")? == 0 {
      return Ok(None);
    }
    let mut obu = vec![first[0]];
    if first[0] & 0x04 != 0 {
      let mut extension = [0u8];
      self
        .input
        .read_exact(&mut extension)
        .context("the stream ends inside an OBU header")
        .code("invalid_bitstream")?;
      obu.push(extension[0]);
    }
    if first[0] & 0x02 == 0 {
      return Err(fail("invalid_bitstream", "an OBU in the low overhead stream has no size field"));
    }
    let size = read_leb128_from(&mut self.input)?
      .context("the stream ends inside an OBU header")
      .code("invalid_bitstream")?;
    write_leb128(&mut obu, size);
    obu.extend(read_exact_unit(&mut self.input, size)?);
    Ok(Some(obu))
  }
}

impl<R: Read> PacketSource for LowOverheadSource<R> {
  fn next_packet(&mut self) -> Result<Option<Packet>> {
    let mut data = self.carry.take().unwrap_or_default();
    while let Some(obu) = self.next_obu()? {
      let is_delimiter = (obu[0] >> 3) & 0x0f == OBU_TEMPORAL_DELIMITER;
      if is_delimiter && !data.is_empty() {
        self.carry = Some(obu);
        break;
      }
      data.extend(obu);
    }
    if data.is_empty() {
      return Ok(None);
    }
    self.index += 1;
    Ok(Some(Packet {
      pts: self.index - 1,
      data,
      units: None,
    }))
  }
}

// An Annex B stream: temporal units made of frame units made of OBUs, each with a
// length in front. OBUs without a size field get one, so the rest of regrain sees the
// same kind of packet as from IVF.
struct AnnexBSource<R: Read> {
  input: R,
  index: u64,
}

impl<R: Read> PacketSource for AnnexBSource<R> {
  fn next_packet(&mut self) -> Result<Option<Packet>> {
    let Some(size) = read_leb128_from(&mut self.input)? else {
      return Ok(None);
    };
    let unit = read_exact_unit(&mut self.input, size)?;
    let mut data = Vec::with_capacity(unit.len() + 16);
    let mut units = Vec::new();
    let mut pos = 0;
    while pos < unit.len() {
      let (frame_size, used) = read_leb128(&unit[pos..]).code("invalid_bitstream")?;
      pos += used;
      let end = pos + frame_size as usize;
      if end > unit.len() {
        return Err(fail("invalid_bitstream", "a frame unit is larger than its temporal unit"));
      }
      let mut count = 0;
      while pos < end {
        let (obu_size, used) = read_leb128(&unit[pos..end]).code("invalid_bitstream")?;
        pos += used;
        let obu_end = pos + obu_size as usize;
        if obu_end > end || obu_size == 0 {
          return Err(fail("invalid_bitstream", "an OBU is larger than its frame unit"));
        }
        let obu = &unit[pos..obu_end];
        let head = header_length(obu[0]).min(obu.len());
        if obu[0] & 0x02 != 0 {
          data.extend_from_slice(obu);
        } else {
          data.push(obu[0] | 0x02);
          data.extend_from_slice(&obu[1..head]);
          write_leb128(&mut data, (obu.len() - head) as u64);
          data.extend_from_slice(&obu[head..]);
        }
        count += 1;
        pos = obu_end;
      }
      units.push(count);
    }
    self.index += 1;
    Ok(Some(Packet {
      pts: self.index - 1,
      data,
      units: Some(units),
    }))
  }
}

// Recognises raw AV1 from its first bytes. None means the data is some other
// container that ffmpeg has to open.
pub fn detect(start: &[u8]) -> Option<Framing> {
  if start.starts_with(b"DKIF") {
    return Some(Framing::Ivf);
  }
  let delimiter = match start.first() {
    Some(0x12) => start.get(1) == Some(&0),
    Some(0x16) => start.get(2) == Some(&0),
    _ => false,
  };
  if delimiter {
    return Some(Framing::LowOverhead);
  }
  let (tu, a) = read_leb128(start).ok()?;
  let (fu, b) = read_leb128(start.get(a..)?).ok()?;
  let (obu, c) = read_leb128(start.get(a + b..)?).ok()?;
  let header = *start.get(a + b + c)?;
  let obu_type = (header >> 3) & 0x0f;
  let plausible = fu + b as u64 <= tu && obu + c as u64 <= fu && obu > 0 && header & 0x81 == 0 && (obu_type == 1 || obu_type == 2);
  plausible.then_some(Framing::AnnexB)
}

pub struct VideoReader {
  source: Box<dyn PacketSource>,
  pending: VecDeque<Packet>,
  pub framing: Framing,
  pub rate: u32,
  pub scale: u32,
  header: [u8; HEADER_SIZE],
}

impl VideoReader {
  pub fn open(mut input: Box<dyn BufRead>, fps: Option<(u32, u32)>) -> Result<VideoReader> {
    let start = input.fill_buf().code("io_error")?.to_vec();
    let framing = detect(&start).ok_or_else(|| fail("unsupported_input", "the input is not an AV1 stream"))?;
    if framing == Framing::Ivf {
      let reader = IvfReader::new(input)?;
      let (rate, scale, header) = match fps {
        Some((num, den)) => {
          let mut header = reader.header;
          header[16..20].copy_from_slice(&num.to_le_bytes());
          header[20..24].copy_from_slice(&den.to_le_bytes());
          (reader.rate, reader.scale, header)
        }
        None => (reader.rate, reader.scale, reader.header),
      };
      return Ok(VideoReader {
        source: Box::new(reader),
        pending: VecDeque::new(),
        framing,
        rate,
        scale,
        header,
      });
    }
    let mut source: Box<dyn PacketSource> = match framing {
      Framing::AnnexB => Box::new(AnnexBSource { input, index: 0 }),
      _ => Box::new(LowOverheadSource {
        input,
        carry: None,
        index: 0,
      }),
    };
    let first = source.next_packet()?;
    let sequence = first.as_ref().and_then(|packet| first_sequence_header(&packet.data));
    let (rate, scale) = fps
      .or_else(|| sequence.as_ref().and_then(|seq| seq.frame_rate))
      .unwrap_or(DEFAULT_FPS);
    let (width, height) = sequence.as_ref().map_or((0, 0), |seq| (seq.max_frame_width, seq.max_frame_height));
    let mut header = [0u8; HEADER_SIZE];
    header[0..4].copy_from_slice(b"DKIF");
    header[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    header[8..12].copy_from_slice(b"AV01");
    header[12..14].copy_from_slice(&(width.min(u16::MAX as u32) as u16).to_le_bytes());
    header[14..16].copy_from_slice(&(height.min(u16::MAX as u32) as u16).to_le_bytes());
    header[16..20].copy_from_slice(&rate.to_le_bytes());
    header[20..24].copy_from_slice(&scale.to_le_bytes());
    Ok(VideoReader {
      source,
      pending: first.into_iter().collect(),
      framing,
      rate,
      scale,
      header,
    })
  }

  pub fn next_packet(&mut self) -> Result<Option<Packet>> {
    match self.pending.pop_front() {
      Some(packet) => Ok(Some(packet)),
      None => self.source.next_packet(),
    }
  }

  pub fn ticks(&self, pts: u64) -> u64 {
    let numerator = pts as u128 * self.scale as u128 * crate::table::TICKS_PER_SECOND as u128;
    numerator.div_ceil(self.rate as u128) as u64
  }

  pub fn ivf_header(&self) -> &[u8; HEADER_SIZE] {
    &self.header
  }
}

fn first_sequence_header(data: &[u8]) -> Option<SequenceHeader> {
  let mut pos = 0;
  while pos < data.len() {
    let head = header_length(data[pos]);
    let (size, used) = read_leb128(data.get(pos + head..)?).ok()?;
    let start = pos + head + used;
    let end = start.checked_add(size as usize)?;
    if (data[pos] >> 3) & 0x0f == 1 {
      return SequenceHeader::parse(data.get(start..end)?).ok();
    }
    pos = end;
  }
  None
}

// Splits a packet with sized OBUs into the byte range of each OBU.
fn split(data: &[u8]) -> Result<Vec<&[u8]>> {
  let mut obus = Vec::new();
  let mut pos = 0;
  while pos < data.len() {
    let head = header_length(data[pos]);
    let (size, used) = read_leb128(data.get(pos + head..).unwrap_or(&[])).code("invalid_bitstream")?;
    let end = pos + head + used + size as usize;
    if end > data.len() {
      return Err(fail("invalid_bitstream", "an OBU is larger than its packet"));
    }
    obus.push(&data[pos..end]);
    pos = end;
  }
  Ok(obus)
}

pub fn write_low_overhead(output: &mut impl Write, data: &[u8]) -> Result<()> {
  let starts_with_delimiter = data.first().is_some_and(|first| (first >> 3) & 0x0f == OBU_TEMPORAL_DELIMITER);
  if !starts_with_delimiter {
    output.write_all(&[0x12, 0x00]).code("io_error")?;
  }
  output.write_all(data).code("io_error")
}

pub fn write_annexb(output: &mut impl Write, data: &[u8], units: &[usize]) -> Result<()> {
  let obus = split(data)?;
  if units.iter().sum::<usize>() != obus.len() {
    return Err(fail(
      "invalid_bitstream",
      "the rewritten temporal unit no longer matches its frame units",
    ));
  }
  let mut temporal = Vec::with_capacity(data.len() + 32);
  let mut next = obus.iter();
  for &count in units {
    let mut frame = Vec::new();
    for obu in next.by_ref().take(count) {
      write_leb128(&mut frame, obu.len() as u64);
      frame.extend_from_slice(obu);
    }
    write_leb128(&mut temporal, frame.len() as u64);
    temporal.extend(frame);
  }
  let mut out = Vec::with_capacity(temporal.len() + 8);
  write_leb128(&mut out, temporal.len() as u64);
  out.extend(temporal);
  output.write_all(&out).code("io_error")
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  fn reader(bytes: Vec<u8>) -> Box<dyn BufRead> {
    Box::new(Cursor::new(bytes))
  }

  #[test]
  fn recognises_framings() {
    assert_eq!(detect(b"DKIF\0\0 \0AV01"), Some(Framing::Ivf));
    assert_eq!(detect(&[0x12, 0x00, 0x0a, 0x0b]), Some(Framing::LowOverhead));
    assert_eq!(
      detect(&[0x0a, 0x09, 0x01, 0x10, 0x07, 0x08, 0, 0, 0, 0, 0, 0]),
      Some(Framing::AnnexB)
    );
    assert_eq!(detect(&[0x1a, 0x45, 0xdf, 0xa3]), None);
    assert_eq!(detect(b"\0\0\0 ftypisom"), None);
  }

  #[test]
  fn reads_low_overhead_temporal_units() {
    let stream = vec![0x12, 0x00, 0x32, 0x01, 0xaa, 0x12, 0x00, 0x32, 0x02, 0xbb, 0xcc];
    let mut video = VideoReader::open(reader(stream), Some((25, 1))).unwrap();
    assert_eq!(video.framing, Framing::LowOverhead);
    let first = video.next_packet().unwrap().unwrap();
    assert_eq!(first.data, vec![0x12, 0x00, 0x32, 0x01, 0xaa]);
    let second = video.next_packet().unwrap().unwrap();
    assert_eq!((second.pts, second.data.len()), (1, 6));
    assert!(video.next_packet().unwrap().is_none());
    assert_eq!(video.ticks(1), 400_000);
  }

  #[test]
  fn annexb_round_trips() {
    let delimiter = [0x10];
    let metadata = [0x28, 0x01, 0x02];
    let mut frame_one = Vec::new();
    for obu in [&delimiter[..], &metadata[..]] {
      write_leb128(&mut frame_one, obu.len() as u64);
      frame_one.extend_from_slice(obu);
    }
    let mut frame_two = Vec::new();
    write_leb128(&mut frame_two, metadata.len() as u64);
    frame_two.extend_from_slice(&metadata);
    let mut temporal = Vec::new();
    for frame in [&frame_one, &frame_two] {
      write_leb128(&mut temporal, frame.len() as u64);
      temporal.extend_from_slice(frame);
    }
    let mut stream = Vec::new();
    write_leb128(&mut stream, temporal.len() as u64);
    stream.extend_from_slice(&temporal);

    let mut video = VideoReader::open(reader(stream.clone()), Some((24, 1))).unwrap();
    assert_eq!(video.framing, Framing::AnnexB);
    let packet = video.next_packet().unwrap().unwrap();
    assert_eq!(packet.units, Some(vec![2, 1]));
    assert_eq!(packet.data, vec![0x12, 0x00, 0x2a, 0x02, 0x01, 0x02, 0x2a, 0x02, 0x01, 0x02]);
    let mut written = Vec::new();
    write_annexb(&mut written, &packet.data, packet.units.as_ref().unwrap()).unwrap();
    let again = VideoReader::open(reader(written), Some((24, 1)))
      .unwrap()
      .next_packet()
      .unwrap()
      .unwrap();
    assert_eq!(again.data, packet.data);
    assert_eq!(again.units, packet.units);
  }
}
