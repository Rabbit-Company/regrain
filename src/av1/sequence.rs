use super::bits::BitReader;
use anyhow::{Result, bail};

pub const SELECT_SCREEN_CONTENT_TOOLS: u32 = 2;
pub const SELECT_INTEGER_MV: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OperatingPoint {
  pub idc: u32,
  pub decoder_model_present: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SequenceHeader {
  pub reduced_still_picture_header: bool,
  pub decoder_model_info_present: bool,
  pub equal_picture_interval: bool,
  pub buffer_removal_time_length: u32,
  pub frame_presentation_time_length: u32,
  // Frames per second as a fraction, when the stream carries timing info.
  pub frame_rate: Option<(u32, u32)>,
  pub operating_points: Vec<OperatingPoint>,
  pub frame_width_bits: u32,
  pub frame_height_bits: u32,
  pub max_frame_width: u32,
  pub max_frame_height: u32,
  pub frame_id_numbers_present: bool,
  pub delta_frame_id_length: u32,
  pub additional_frame_id_length: u32,
  pub use_128x128_superblock: bool,
  pub enable_warped_motion: bool,
  pub enable_order_hint: bool,
  pub enable_ref_frame_mvs: bool,
  pub force_screen_content_tools: u32,
  pub force_integer_mv: u32,
  pub order_hint_bits: u32,
  pub enable_superres: bool,
  pub enable_cdef: bool,
  pub enable_restoration: bool,
  pub bit_depth: u8,
  pub mono_chrome: bool,
  pub subsampling_x: bool,
  pub subsampling_y: bool,
  pub separate_uv_delta_q: bool,
  pub film_grain_params_present: bool,
  pub film_grain_bit: usize,
}

impl SequenceHeader {
  pub fn num_planes(&self) -> usize {
    if self.mono_chrome { 1 } else { 3 }
  }

  pub fn parse(data: &[u8]) -> Result<Self> {
    let mut r = BitReader::new(data);
    let mut seq = SequenceHeader::default();
    let profile = r.bits(3)?;
    if profile > 2 {
      bail!("unsupported AV1 profile {profile}");
    }
    let _still_picture = r.bit()?;
    seq.reduced_still_picture_header = r.bit()?;
    if seq.reduced_still_picture_header {
      r.skip(5)?;
      seq.operating_points.push(OperatingPoint::default());
    } else {
      let mut buffer_delay_length = 0;
      if r.bit()? {
        let units_per_tick = r.bits(32)?;
        let time_scale = r.bits(32)?;
        seq.equal_picture_interval = r.bit()?;
        let mut ticks_per_picture = 1u64;
        if seq.equal_picture_interval {
          ticks_per_picture = r.uvlc()? as u64 + 1;
        }
        let denominator = units_per_tick as u64 * ticks_per_picture;
        if time_scale > 0 && denominator > 0 && denominator <= u32::MAX as u64 {
          seq.frame_rate = Some((time_scale, denominator as u32));
        }
        seq.decoder_model_info_present = r.bit()?;
        if seq.decoder_model_info_present {
          buffer_delay_length = r.bits(5)? + 1;
          r.skip(32)?;
          seq.buffer_removal_time_length = r.bits(5)? + 1;
          seq.frame_presentation_time_length = r.bits(5)? + 1;
        }
      }
      let initial_display_delay_present = r.bit()?;
      let count = r.bits(5)? + 1;
      for _ in 0..count {
        let mut point = OperatingPoint {
          idc: r.bits(12)?,
          decoder_model_present: false,
        };
        let level = r.bits(5)?;
        if level > 7 {
          r.skip(1)?;
        }
        if seq.decoder_model_info_present {
          point.decoder_model_present = r.bit()?;
          if point.decoder_model_present {
            r.skip(2 * buffer_delay_length as usize + 1)?;
          }
        }
        if initial_display_delay_present && r.bit()? {
          r.skip(4)?;
        }
        seq.operating_points.push(point);
      }
    }
    seq.frame_width_bits = r.bits(4)? + 1;
    seq.frame_height_bits = r.bits(4)? + 1;
    seq.max_frame_width = r.bits(seq.frame_width_bits)? + 1;
    seq.max_frame_height = r.bits(seq.frame_height_bits)? + 1;
    if !seq.reduced_still_picture_header {
      seq.frame_id_numbers_present = r.bit()?;
    }
    if seq.frame_id_numbers_present {
      seq.delta_frame_id_length = r.bits(4)? + 2;
      seq.additional_frame_id_length = r.bits(3)? + 1;
    }
    seq.use_128x128_superblock = r.bit()?;
    r.skip(2)?;
    seq.force_screen_content_tools = SELECT_SCREEN_CONTENT_TOOLS;
    seq.force_integer_mv = SELECT_INTEGER_MV;
    if !seq.reduced_still_picture_header {
      r.skip(2)?;
      seq.enable_warped_motion = r.bit()?;
      r.skip(1)?;
      seq.enable_order_hint = r.bit()?;
      if seq.enable_order_hint {
        r.skip(1)?;
        seq.enable_ref_frame_mvs = r.bit()?;
      }
      if !r.bit()? {
        seq.force_screen_content_tools = r.bits(1)?;
      }
      if seq.force_screen_content_tools > 0 && !r.bit()? {
        seq.force_integer_mv = r.bits(1)?;
      }
      if seq.enable_order_hint {
        seq.order_hint_bits = r.bits(3)? + 1;
      }
    }
    seq.enable_superres = r.bit()?;
    seq.enable_cdef = r.bit()?;
    seq.enable_restoration = r.bit()?;

    let high_bitdepth = r.bit()?;
    seq.bit_depth = if profile == 2 && high_bitdepth {
      if r.bit()? { 12 } else { 10 }
    } else if high_bitdepth {
      10
    } else {
      8
    };
    seq.mono_chrome = if profile == 1 { false } else { r.bit()? };
    let (mut primaries, mut transfer, mut matrix) = (2, 2, 2);
    if r.bit()? {
      primaries = r.bits(8)?;
      transfer = r.bits(8)?;
      matrix = r.bits(8)?;
    }
    if seq.mono_chrome {
      r.skip(1)?;
      seq.subsampling_x = true;
      seq.subsampling_y = true;
    } else if primaries == 1 && transfer == 13 && matrix == 0 {
      seq.separate_uv_delta_q = r.bit()?;
    } else {
      r.skip(1)?;
      match profile {
        0 => {
          seq.subsampling_x = true;
          seq.subsampling_y = true;
        }
        1 => {}
        _ => {
          if seq.bit_depth == 12 {
            seq.subsampling_x = r.bit()?;
            if seq.subsampling_x {
              seq.subsampling_y = r.bit()?;
            }
          } else {
            seq.subsampling_x = true;
          }
        }
      }
      if seq.subsampling_x && seq.subsampling_y {
        r.skip(2)?;
      }
      seq.separate_uv_delta_q = r.bit()?;
    }
    seq.film_grain_bit = r.position();
    seq.film_grain_params_present = r.bit()?;
    Ok(seq)
  }
}
