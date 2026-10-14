use super::bits::BitReader;
use super::sequence::{SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS, SequenceHeader};
use anyhow::{Result, bail};

pub const NUM_REF_FRAMES: usize = 8;
pub const REFS_PER_FRAME: usize = 7;
const PRIMARY_REF_NONE: u32 = 7;
const KEY_FRAME: u32 = 0;
const INTER_FRAME: u32 = 1;
const INTRA_ONLY_FRAME: u32 = 2;
const SWITCH_FRAME: u32 = 3;
const SEG_FEATURE_BITS: [u32; 8] = [8, 6, 6, 6, 6, 3, 0, 0];
const SEG_FEATURE_SIGNED: [bool; 8] = [true, true, true, true, true, false, false, false];
const SEG_FEATURE_MAX: [i32; 8] = [255, 63, 63, 63, 63, 7, 0, 0];

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Segmentation {
  enabled: [[bool; 8]; 8],
  data: [[i32; 8]; 8],
}

#[derive(Clone, Copy, Debug, Default)]
struct RefSlot {
  frame_type: u32,
  upscaled_width: u32,
  frame_height: u32,
  order_hint: u32,
  segmentation: Segmentation,
}

#[derive(Clone, Debug, Default)]
pub struct FrameState {
  slots: [RefSlot; NUM_REF_FRAMES],
}

#[derive(Clone, Debug, Default)]
pub struct FrameHeader {
  pub show_existing_frame: bool,
  pub frame_to_show: usize,
  pub frame_type: u32,
  pub show_frame: bool,
  pub showable_frame: bool,
  pub refresh_frame_flags: u8,
  pub ref_frame_idx: [usize; REFS_PER_FRAME],
  pub tile_cols: u32,
  pub tile_rows: u32,
  pub tile_cols_log2: u32,
  pub tile_rows_log2: u32,
  pub grain_bit: usize,
  upscaled_width: u32,
  frame_width: u32,
  frame_height: u32,
  order_hint: u32,
  segmentation: Segmentation,
}

impl FrameHeader {
  pub fn is_key(&self) -> bool {
    self.frame_type == KEY_FRAME
  }

  pub fn is_inter(&self) -> bool {
    self.frame_type == INTER_FRAME
  }

  pub fn grain_allowed(&self) -> bool {
    self.show_frame || self.showable_frame
  }

  pub fn num_tiles(&self) -> u32 {
    self.tile_cols * self.tile_rows
  }
}

impl FrameState {
  pub fn update(&mut self, header: &FrameHeader) {
    if header.show_existing_frame {
      if header.frame_type == KEY_FRAME {
        let slot = self.slots[header.frame_to_show];
        self.slots = [slot; NUM_REF_FRAMES];
      }
      return;
    }
    let slot = RefSlot {
      frame_type: header.frame_type,
      upscaled_width: header.upscaled_width,
      frame_height: header.frame_height,
      order_hint: header.order_hint,
      segmentation: header.segmentation,
    };
    for (i, target) in self.slots.iter_mut().enumerate() {
      if header.refresh_frame_flags >> i & 1 == 1 {
        *target = slot;
      }
    }
  }

  pub fn parse(&mut self, r: &mut BitReader, seq: &SequenceHeader, temporal_id: u32, spatial_id: u32) -> Result<FrameHeader> {
    let mut h = FrameHeader::default();
    let id_len = if seq.frame_id_numbers_present {
      seq.additional_frame_id_length + seq.delta_frame_id_length
    } else {
      0
    };
    let frame_is_intra;
    let error_resilient_mode;
    if seq.reduced_still_picture_header {
      h.frame_type = KEY_FRAME;
      h.show_frame = true;
      frame_is_intra = true;
      error_resilient_mode = true;
    } else {
      h.show_existing_frame = r.bit()?;
      if h.show_existing_frame {
        h.frame_to_show = r.bits(3)? as usize;
        if seq.decoder_model_info_present && !seq.equal_picture_interval {
          r.skip(seq.frame_presentation_time_length as usize)?;
        }
        if seq.frame_id_numbers_present {
          r.skip(id_len as usize)?;
        }
        h.frame_type = self.slots[h.frame_to_show].frame_type;
        h.show_frame = true;
        h.grain_bit = r.position();
        return Ok(h);
      }
      h.frame_type = r.bits(2)?;
      frame_is_intra = h.frame_type == INTRA_ONLY_FRAME || h.frame_type == KEY_FRAME;
      h.show_frame = r.bit()?;
      if h.show_frame && seq.decoder_model_info_present && !seq.equal_picture_interval {
        r.skip(seq.frame_presentation_time_length as usize)?;
      }
      h.showable_frame = if h.show_frame { h.frame_type != KEY_FRAME } else { r.bit()? };
      error_resilient_mode = if h.frame_type == SWITCH_FRAME || (h.frame_type == KEY_FRAME && h.show_frame) {
        true
      } else {
        r.bit()?
      };
    }
    if h.frame_type == KEY_FRAME && h.show_frame {
      for slot in &mut self.slots {
        slot.order_hint = 0;
      }
    }
    let disable_cdf_update = r.bit()?;
    let allow_screen_content_tools = if seq.force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS {
      r.bits(1)?
    } else {
      seq.force_screen_content_tools
    };
    let mut force_integer_mv = 0;
    if allow_screen_content_tools != 0 {
      force_integer_mv = if seq.force_integer_mv == SELECT_INTEGER_MV {
        r.bits(1)?
      } else {
        seq.force_integer_mv
      };
    }
    if frame_is_intra {
      force_integer_mv = 1;
    }
    if seq.frame_id_numbers_present {
      r.skip(id_len as usize)?;
    }
    let frame_size_override_flag = if h.frame_type == SWITCH_FRAME {
      true
    } else if seq.reduced_still_picture_header {
      false
    } else {
      r.bit()?
    };
    h.order_hint = r.bits(seq.order_hint_bits)?;
    let primary_ref_frame = if frame_is_intra || error_resilient_mode {
      PRIMARY_REF_NONE
    } else {
      r.bits(3)?
    };
    if seq.decoder_model_info_present && r.bit()? {
      for point in &seq.operating_points {
        if point.decoder_model_present {
          let in_temporal = point.idc >> temporal_id & 1 == 1;
          let in_spatial = point.idc >> (spatial_id + 8) & 1 == 1;
          if point.idc == 0 || (in_temporal && in_spatial) {
            r.skip(seq.buffer_removal_time_length as usize)?;
          }
        }
      }
    }
    h.refresh_frame_flags = if h.frame_type == SWITCH_FRAME || (h.frame_type == KEY_FRAME && h.show_frame) {
      0xff
    } else {
      r.bits(8)? as u8
    };
    if (!frame_is_intra || h.refresh_frame_flags != 0xff) && error_resilient_mode && seq.enable_order_hint {
      for slot in &mut self.slots {
        slot.order_hint = r.bits(seq.order_hint_bits)?;
      }
    }

    let mut allow_intrabc = false;
    let mut allow_high_precision_mv = false;
    if frame_is_intra {
      self.frame_size(r, seq, &mut h, frame_size_override_flag)?;
      Self::render_size(r)?;
      if allow_screen_content_tools != 0 && h.upscaled_width == h.frame_width {
        allow_intrabc = r.bit()?;
      }
    } else {
      let mut short_signaling = false;
      if seq.enable_order_hint {
        short_signaling = r.bit()?;
        if short_signaling {
          let last = r.bits(3)? as usize;
          let gold = r.bits(3)? as usize;
          h.ref_frame_idx = self.frame_refs(seq, h.order_hint, last, gold);
        }
      }
      for i in 0..REFS_PER_FRAME {
        if !short_signaling {
          h.ref_frame_idx[i] = r.bits(3)? as usize;
        }
        if seq.frame_id_numbers_present {
          r.skip(seq.delta_frame_id_length as usize)?;
        }
      }
      if frame_size_override_flag && !error_resilient_mode {
        let mut found = false;
        for i in 0..REFS_PER_FRAME {
          if r.bit()? {
            let slot = &self.slots[h.ref_frame_idx[i]];
            h.upscaled_width = slot.upscaled_width;
            h.frame_width = slot.upscaled_width;
            h.frame_height = slot.frame_height;
            found = true;
            break;
          }
        }
        if found {
          Self::superres(r, seq, &mut h)?;
        } else {
          self.frame_size(r, seq, &mut h, frame_size_override_flag)?;
          Self::render_size(r)?;
        }
      } else {
        self.frame_size(r, seq, &mut h, frame_size_override_flag)?;
        Self::render_size(r)?;
      }
      if force_integer_mv == 0 {
        allow_high_precision_mv = r.bit()?;
      }
      if !r.bit()? {
        r.skip(2)?;
      }
      r.skip(1)?;
      if !error_resilient_mode && seq.enable_ref_frame_mvs {
        r.skip(1)?;
      }
    }
    if !seq.reduced_still_picture_header && !disable_cdf_update {
      r.skip(1)?;
    }
    h.segmentation = if primary_ref_frame == PRIMARY_REF_NONE {
      Segmentation::default()
    } else {
      self.slots[h.ref_frame_idx[primary_ref_frame as usize]].segmentation
    };

    Self::tile_info(r, seq, &mut h)?;

    let base_q_idx = r.bits(8)?;
    let mut delta_q_nonzero = Self::delta_q(r)? != 0;
    if !seq.mono_chrome {
      let diff_uv_delta = if seq.separate_uv_delta_q { r.bit()? } else { false };
      delta_q_nonzero |= Self::delta_q(r)? != 0;
      delta_q_nonzero |= Self::delta_q(r)? != 0;
      if diff_uv_delta {
        delta_q_nonzero |= Self::delta_q(r)? != 0;
        delta_q_nonzero |= Self::delta_q(r)? != 0;
      }
    }
    if r.bit()? {
      r.skip(8)?;
      if seq.separate_uv_delta_q {
        r.skip(4)?;
      }
    }

    let segmentation_enabled = r.bit()?;
    if segmentation_enabled {
      let update_data = if primary_ref_frame == PRIMARY_REF_NONE {
        true
      } else {
        if r.bit()? {
          r.skip(1)?;
        }
        r.bit()?
      };
      if update_data {
        for segment in 0..8 {
          for feature in 0..8 {
            let enabled = r.bit()?;
            let mut value = 0;
            if enabled {
              let bits = SEG_FEATURE_BITS[feature];
              let limit = SEG_FEATURE_MAX[feature];
              if SEG_FEATURE_SIGNED[feature] {
                value = r.signed(1 + bits)?.clamp(-limit, limit);
              } else if bits > 0 {
                value = (r.bits(bits)? as i32).clamp(0, limit);
              }
            }
            h.segmentation.enabled[segment][feature] = enabled;
            h.segmentation.data[segment][feature] = value;
          }
        }
      }
    } else {
      h.segmentation = Segmentation::default();
    }

    let mut delta_q_present = false;
    if base_q_idx > 0 {
      delta_q_present = r.bit()?;
    }
    if delta_q_present {
      r.skip(2)?;
      if !allow_intrabc && r.bit()? {
        r.skip(3)?;
      }
    }

    let mut coded_lossless = !delta_q_nonzero;
    if coded_lossless {
      for segment in 0..8 {
        let mut qindex = base_q_idx as i32;
        if segmentation_enabled && h.segmentation.enabled[segment][0] {
          qindex = (qindex + h.segmentation.data[segment][0]).clamp(0, 255);
        }
        if qindex != 0 {
          coded_lossless = false;
          break;
        }
      }
    }
    let all_lossless = coded_lossless && h.frame_width == h.upscaled_width;

    if !coded_lossless && !allow_intrabc {
      let level0 = r.bits(6)?;
      let level1 = r.bits(6)?;
      if !seq.mono_chrome && (level0 != 0 || level1 != 0) {
        r.skip(12)?;
      }
      r.skip(3)?;
      if r.bit()? && r.bit()? {
        for _ in 0..10 {
          if r.bit()? {
            r.skip(7)?;
          }
        }
      }
    }
    if !coded_lossless && !allow_intrabc && seq.enable_cdef {
      r.skip(2)?;
      let cdef_bits = r.bits(2)?;
      let per_strength = if seq.mono_chrome { 6 } else { 12 };
      r.skip(per_strength << cdef_bits)?;
    }
    if !all_lossless && !allow_intrabc && seq.enable_restoration {
      let mut uses_lr = false;
      let mut uses_chroma_lr = false;
      for plane in 0..seq.num_planes() {
        if r.bits(2)? != 0 {
          uses_lr = true;
          uses_chroma_lr |= plane > 0;
        }
      }
      if uses_lr {
        let unit_shift = r.bit()?;
        if unit_shift && !seq.use_128x128_superblock {
          r.skip(1)?;
        }
        if seq.subsampling_x && seq.subsampling_y && uses_chroma_lr {
          r.skip(1)?;
        }
      }
    }
    if !coded_lossless {
      r.skip(1)?;
    }
    let reference_select = if frame_is_intra { false } else { r.bit()? };
    if self.skip_mode_allowed(seq, &h, frame_is_intra, reference_select) {
      r.skip(1)?;
    }
    if !frame_is_intra && !error_resilient_mode && seq.enable_warped_motion {
      r.skip(1)?;
    }
    r.skip(1)?;
    if !frame_is_intra {
      Self::global_motion(r, allow_high_precision_mv)?;
    }
    h.grain_bit = r.position();
    Ok(h)
  }

  fn frame_size(&self, r: &mut BitReader, seq: &SequenceHeader, h: &mut FrameHeader, override_flag: bool) -> Result<()> {
    if override_flag {
      h.frame_width = r.bits(seq.frame_width_bits)? + 1;
      h.frame_height = r.bits(seq.frame_height_bits)? + 1;
    } else {
      h.frame_width = seq.max_frame_width;
      h.frame_height = seq.max_frame_height;
    }
    Self::superres(r, seq, h)
  }

  fn superres(r: &mut BitReader, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<()> {
    let mut denom = 8;
    if seq.enable_superres && r.bit()? {
      denom = r.bits(3)? + 9;
    }
    h.upscaled_width = h.frame_width;
    h.frame_width = (h.upscaled_width * 8 + denom / 2) / denom;
    Ok(())
  }

  fn render_size(r: &mut BitReader) -> Result<()> {
    if r.bit()? {
      r.skip(32)?;
    }
    Ok(())
  }

  fn delta_q(r: &mut BitReader) -> Result<i32> {
    if r.bit()? { r.signed(7) } else { Ok(0) }
  }

  fn tile_info(r: &mut BitReader, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<()> {
    let mi_cols = 2 * ((h.frame_width + 7) >> 3);
    let mi_rows = 2 * ((h.frame_height + 7) >> 3);
    let (sb_cols, sb_rows, sb_shift) = if seq.use_128x128_superblock {
      ((mi_cols + 31) >> 5, (mi_rows + 31) >> 5, 5)
    } else {
      ((mi_cols + 15) >> 4, (mi_rows + 15) >> 4, 4)
    };
    if sb_cols == 0 || sb_rows == 0 {
      bail!("invalid frame size in frame header");
    }
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = 4096 >> sb_size;
    let mut max_tile_area_sb = (4096 * 2304) >> (2 * sb_size);
    let tile_log2 = |blk: u32, target: u32| {
      let mut k = 0;
      while (blk << k) < target {
        k += 1;
      }
      k
    };
    let min_log2_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_cols = tile_log2(1, sb_cols.min(64));
    let max_log2_rows = tile_log2(1, sb_rows.min(64));
    let min_log2_tiles = min_log2_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));

    if r.bit()? {
      h.tile_cols_log2 = min_log2_cols;
      while h.tile_cols_log2 < max_log2_cols && r.bit()? {
        h.tile_cols_log2 += 1;
      }
      let tile_width_sb = (sb_cols + (1 << h.tile_cols_log2) - 1) >> h.tile_cols_log2;
      h.tile_cols = sb_cols.div_ceil(tile_width_sb);
      let min_log2_rows = min_log2_tiles.saturating_sub(h.tile_cols_log2);
      h.tile_rows_log2 = min_log2_rows;
      while h.tile_rows_log2 < max_log2_rows && r.bit()? {
        h.tile_rows_log2 += 1;
      }
      let tile_height_sb = (sb_rows + (1 << h.tile_rows_log2) - 1) >> h.tile_rows_log2;
      h.tile_rows = sb_rows.div_ceil(tile_height_sb);
    } else {
      let mut widest = 0;
      let mut start = 0;
      h.tile_cols = 0;
      while start < sb_cols {
        let size = r.ns((sb_cols - start).min(max_tile_width_sb))? + 1;
        widest = widest.max(size);
        start += size;
        h.tile_cols += 1;
      }
      h.tile_cols_log2 = tile_log2(1, h.tile_cols);
      max_tile_area_sb = if min_log2_tiles > 0 {
        (sb_rows * sb_cols) >> (min_log2_tiles + 1)
      } else {
        sb_rows * sb_cols
      };
      let max_tile_height_sb = (max_tile_area_sb / widest).max(1);
      start = 0;
      h.tile_rows = 0;
      while start < sb_rows {
        let size = r.ns((sb_rows - start).min(max_tile_height_sb))? + 1;
        start += size;
        h.tile_rows += 1;
      }
      h.tile_rows_log2 = tile_log2(1, h.tile_rows);
    }
    if h.tile_cols_log2 > 0 || h.tile_rows_log2 > 0 {
      r.skip((h.tile_rows_log2 + h.tile_cols_log2) as usize + 2)?;
    }
    Ok(())
  }

  fn relative_dist(seq: &SequenceHeader, a: u32, b: u32) -> i32 {
    if !seq.enable_order_hint {
      return 0;
    }
    let diff = a as i32 - b as i32;
    let m = 1 << (seq.order_hint_bits - 1);
    (diff & (m - 1)) - (diff & m)
  }

  fn skip_mode_allowed(&self, seq: &SequenceHeader, h: &FrameHeader, frame_is_intra: bool, reference_select: bool) -> bool {
    if frame_is_intra || !reference_select || !seq.enable_order_hint {
      return false;
    }
    let hints: Vec<u32> = h.ref_frame_idx.iter().map(|&idx| self.slots[idx].order_hint).collect();
    let mut forward: Option<u32> = None;
    let mut backward: Option<u32> = None;
    for &hint in &hints {
      let dist = Self::relative_dist(seq, hint, h.order_hint);
      if dist < 0 {
        if forward.is_none_or(|current| Self::relative_dist(seq, hint, current) > 0) {
          forward = Some(hint);
        }
      } else if dist > 0 && backward.is_none_or(|current| Self::relative_dist(seq, hint, current) < 0) {
        backward = Some(hint);
      }
    }
    match (forward, backward) {
      (None, _) => false,
      (Some(_), Some(_)) => true,
      (Some(forward), None) => hints.iter().any(|&hint| Self::relative_dist(seq, hint, forward) < 0),
    }
  }

  fn frame_refs(&self, seq: &SequenceHeader, order_hint: u32, last: usize, gold: usize) -> [usize; REFS_PER_FRAME] {
    let mut refs: [Option<usize>; REFS_PER_FRAME] = [None; REFS_PER_FRAME];
    refs[0] = Some(last);
    refs[3] = Some(gold);
    let mut used = [false; NUM_REF_FRAMES];
    used[last] = true;
    used[gold] = true;
    let current = 1i32 << (seq.order_hint_bits - 1);
    let shifted: Vec<i32> = self
      .slots
      .iter()
      .map(|slot| current + Self::relative_dist(seq, slot.order_hint, order_hint))
      .collect();

    let find = |used: &[bool; NUM_REF_FRAMES], backward: bool, latest: bool| -> Option<usize> {
      let mut best: Option<(usize, i32)> = None;
      for i in 0..NUM_REF_FRAMES {
        let hint = shifted[i];
        if used[i] || (hint >= current) != backward {
          continue;
        }
        let better = match best {
          None => true,
          Some((_, value)) => {
            if latest {
              hint >= value
            } else {
              hint < value
            }
          }
        };
        if better {
          best = Some((i, hint));
        }
      }
      best.map(|(i, _)| i)
    };

    for (target, latest) in [(6, true), (4, false), (5, false)] {
      if let Some(slot) = find(&used, true, latest) {
        refs[target] = Some(slot);
        used[slot] = true;
      }
    }
    for target in [1, 2, 4, 5, 6] {
      if refs[target].is_none()
        && let Some(slot) = find(&used, false, true)
      {
        refs[target] = Some(slot);
        used[slot] = true;
      }
    }
    let mut earliest = 0;
    for i in 1..NUM_REF_FRAMES {
      if shifted[i] < shifted[earliest] {
        earliest = i;
      }
    }
    refs.map(|slot| slot.unwrap_or(earliest))
  }

  fn global_motion(r: &mut BitReader, allow_high_precision_mv: bool) -> Result<()> {
    for _ in 0..REFS_PER_FRAME {
      let mut kind = 0;
      if r.bit()? {
        kind = if r.bit()? {
          2
        } else if r.bit()? {
          1
        } else {
          3
        };
      }
      if kind >= 2 {
        Self::global_param(r, 12)?;
        Self::global_param(r, 12)?;
        if kind == 3 {
          Self::global_param(r, 12)?;
          Self::global_param(r, 12)?;
        }
      }
      if kind >= 1 {
        let abs_bits = if kind == 1 { 9 - u32::from(!allow_high_precision_mv) } else { 12 };
        Self::global_param(r, abs_bits)?;
        Self::global_param(r, abs_bits)?;
      }
    }
    Ok(())
  }

  fn global_param(r: &mut BitReader, abs_bits: u32) -> Result<()> {
    let num_syms = (2u32 << abs_bits) + 1;
    let mut i = 0;
    let mut mk = 0;
    let k = 3;
    loop {
      let b2 = if i > 0 { k + i - 1 } else { k };
      let a = 1u32 << b2;
      if num_syms <= mk + 3 * a {
        r.ns(num_syms - mk)?;
        return Ok(());
      }
      if r.bit()? {
        i += 1;
        mk += a;
      } else {
        r.skip(b2 as usize)?;
        return Ok(());
      }
    }
  }
}
