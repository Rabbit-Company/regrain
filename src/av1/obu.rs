use super::bits::{BitReader, read_leb128};
use super::frame::{FrameHeader, FrameState};
use super::sequence::SequenceHeader;
use crate::grain::{ChromaFormat, GrainParams};
use anyhow::{Context, Result, bail};
use std::ops::Range;

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_TILE_GROUP: u8 = 4;
pub const OBU_FRAME: u8 = 6;
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;

#[derive(Clone, Debug, PartialEq)]
pub enum CodedGrain {
  Absent,
  NotApplied,
  Reference { seed: u16, slot: usize },
  Full { seed: u16, params: GrainParams },
}

#[derive(Clone, Debug)]
pub struct CodedFrame {
  pub header: FrameHeader,
  pub grain: CodedGrain,
  pub header_end_bit: usize,
}

#[derive(Clone, Debug)]
pub enum ObuKind {
  Sequence(SequenceHeader),
  Frame(Box<CodedFrame>),
  FrameCopy,
  Other,
}

#[derive(Clone, Debug)]
pub struct Obu {
  pub obu_type: u8,
  pub has_size: bool,
  pub prefix: Range<usize>,
  pub payload: Range<usize>,
  pub kind: ObuKind,
}

#[derive(Default)]
pub struct StreamParser {
  sequence: Option<SequenceHeader>,
  state: FrameState,
  current: Option<FrameHeader>,
}

impl StreamParser {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn format(&self) -> Option<ChromaFormat> {
    self.sequence.as_ref().map(|seq| ChromaFormat {
      mono_chrome: seq.mono_chrome,
      subsampling_x: seq.subsampling_x,
      subsampling_y: seq.subsampling_y,
    })
  }

  pub fn parse_packet(&mut self, data: &[u8]) -> Result<Vec<Obu>> {
    let mut obus = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
      let header = data[pos];
      if header & 0x80 != 0 {
        bail!("invalid OBU header");
      }
      let obu_type = (header >> 3) & 0x0f;
      let has_extension = header & 0x04 != 0;
      let has_size = header & 0x02 != 0;
      let mut cursor = pos + 1;
      let (mut temporal_id, mut spatial_id) = (0, 0);
      if has_extension {
        let Some(&extension) = data.get(cursor) else {
          bail!("truncated OBU extension header");
        };
        temporal_id = (extension >> 5) as u32;
        spatial_id = ((extension >> 3) & 3) as u32;
        cursor += 1;
      }
      let prefix = pos..cursor;
      let size = if has_size {
        let (size, used) = read_leb128(&data[cursor..])?;
        cursor += used;
        size as usize
      } else {
        data.len() - cursor
      };
      if cursor + size > data.len() {
        bail!("OBU size exceeds the packet");
      }
      let payload = cursor..cursor + size;
      let bytes = &data[payload.clone()];
      let kind = match obu_type {
        OBU_SEQUENCE_HEADER => {
          let sequence = SequenceHeader::parse(bytes).context("failed to parse the sequence header")?;
          self.sequence = Some(sequence.clone());
          ObuKind::Sequence(sequence)
        }
        OBU_TEMPORAL_DELIMITER => {
          self.current = None;
          ObuKind::Other
        }
        OBU_FRAME_HEADER | OBU_REDUNDANT_FRAME_HEADER | OBU_FRAME => {
          if self.current.is_some() && obu_type != OBU_FRAME {
            ObuKind::FrameCopy
          } else {
            let frame = self
              .parse_frame(bytes, temporal_id, spatial_id)
              .context("failed to parse a frame header")?;
            if obu_type == OBU_FRAME && !frame.header.show_existing_frame {
              let mut r = BitReader::new(bytes);
              r.skip(frame.header_end_bit.div_ceil(8) * 8)?;
              self.tile_group(&mut r)?;
            }
            ObuKind::Frame(Box::new(frame))
          }
        }
        OBU_TILE_GROUP => {
          self.tile_group(&mut BitReader::new(bytes))?;
          ObuKind::Other
        }
        _ => ObuKind::Other,
      };
      obus.push(Obu {
        obu_type,
        has_size,
        prefix,
        payload: payload.clone(),
        kind,
      });
      pos = payload.end;
    }
    Ok(obus)
  }

  fn parse_frame(&mut self, bytes: &[u8], temporal_id: u32, spatial_id: u32) -> Result<CodedFrame> {
    let Some(seq) = self.sequence.as_ref() else {
      bail!("frame header found before any sequence header");
    };
    let mut r = BitReader::new(bytes);
    let header = self.state.parse(&mut r, seq, temporal_id, spatial_id)?;
    let mut grain = CodedGrain::Absent;
    if !header.show_existing_frame && seq.film_grain_params_present && header.grain_allowed() {
      grain = CodedGrain::NotApplied;
      if r.bit()? {
        let seed = r.bits(16)? as u16;
        let update = if header.is_inter() { r.bit()? } else { true };
        grain = if update {
          let format = ChromaFormat {
            mono_chrome: seq.mono_chrome,
            subsampling_x: seq.subsampling_x,
            subsampling_y: seq.subsampling_y,
          };
          CodedGrain::Full {
            seed,
            params: GrainParams::read(&mut r, format)?,
          }
        } else {
          CodedGrain::Reference {
            seed,
            slot: r.bits(3)? as usize,
          }
        };
      }
    }
    let header_end_bit = r.position();
    self.state.update(&header);
    self.current = if header.show_existing_frame { None } else { Some(header.clone()) };
    Ok(CodedFrame {
      header,
      grain,
      header_end_bit,
    })
  }

  fn tile_group(&mut self, r: &mut BitReader) -> Result<()> {
    let Some(header) = self.current.as_ref() else {
      return Ok(());
    };
    let tiles = header.num_tiles();
    let mut last = tiles - 1;
    if tiles > 1 && r.bit()? {
      let bits = header.tile_cols_log2 + header.tile_rows_log2;
      r.skip(bits as usize)?;
      last = r.bits(bits)?;
    }
    if last == tiles - 1 {
      self.current = None;
    }
    Ok(())
  }
}
