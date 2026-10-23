use crate::diff::synth::simulate;
use crate::table::{GrainTable, OPEN_END, TICKS_PER_SECOND};
use crate::template::extremes;
use rabbit_av1::grain::GrainParams;

// Stats describe a 10 bit 4:2:0 decode, the usual target of AV1 encodes.
const DEPTH: u8 = 10;

// Scaling function values at each of the 256 brightness levels, built from the
// points with the integer steps of an AV1 decoder (section 7.18.3.5).
fn scaling_lut(points: &[[u8; 2]]) -> [i32; 256] {
  let mut lut = [0; 256];
  let (Some(first), Some(last)) = (points.first(), points.last()) else {
    return lut;
  };
  lut[..first[0] as usize].fill(first[1] as i32);
  for pair in points.windows(2) {
    let delta_x = pair[1][0] as i32 - pair[0][0] as i32;
    let delta_y = pair[1][1] as i32 - pair[0][1] as i32;
    let delta = delta_y * ((65536 + (delta_x >> 1)) / delta_x);
    for x in 0..delta_x {
      lut[pair[0][0] as usize + x as usize] = pair[0][1] as i32 + ((x * delta + 32768) >> 16);
    }
  }
  lut[last[0] as usize..].fill(last[1] as i32);
  lut
}

// The decoder's scale_lut: picks the scaling value for a sample of the decoded
// bit depth, between two entries of the 8 bit table.
fn scale_at(lut: &[i32; 256], index: i32) -> i32 {
  let shift = DEPTH as u32 - 8;
  let x = (index >> shift) as usize;
  let rem = index - ((x as i32) << shift);
  if x == 255 {
    lut[x]
  } else {
    lut[x] + (((lut[x + 1] - lut[x]) * rem + (1 << (shift - 1))) >> shift)
  }
}

// Scaling value the decoder applies at each brightness level for luma, Cb and Cr.
// Chroma is taken as neutral, as on grey picture areas, since the decoder picks
// the chroma scaling from the luma and chroma value together.
fn scaling_per_level(params: &GrainParams, active: [bool; 2]) -> [[i32; 256]; 3] {
  let luma_lut = scaling_lut(&params.scaling_points_y);
  let shift = DEPTH as u32 - 8;
  let neutral = 128 << shift;
  let max = (1 << DEPTH) - 1;
  let mut out = [[0; 256]; 3];
  for level in 0..256 {
    out[0][level] = scale_at(&luma_lut, (level as i32) << shift);
  }
  let chroma = [
    (&params.scaling_points_cb, params.cb_mult, params.cb_luma_mult, params.cb_offset),
    (&params.scaling_points_cr, params.cr_mult, params.cr_luma_mult, params.cr_offset),
  ];
  for (index, (points, mult, luma_mult, offset)) in chroma.into_iter().enumerate() {
    if !active[index] {
      continue;
    }
    let lut = if params.chroma_scaling_from_luma {
      luma_lut
    } else {
      scaling_lut(points)
    };
    for level in 0..256 {
      let luma = (level as i32) << shift;
      let merged = if params.chroma_scaling_from_luma {
        luma
      } else {
        let combined = luma * (luma_mult as i32 - 128) + neutral * (mult as i32 - 128);
        ((combined >> 6) + ((offset as i32 - 256) << shift)).clamp(0, max)
      };
      out[index + 1][level] = scale_at(&lut, merged);
    }
  }
  out
}

pub struct EntryStats {
  pub start: f64,
  pub end: Option<f64>,
  pub seed: u16,
  pub luma: [f64; 3],
  pub luma_mean: f64,
  pub chroma: [f64; 2],
  pub lag: u8,
  pub correlation: f64,
  // Grain deviation at each brightness level for luma, Cb and Cr.
  pub deviation: [[f64; 256]; 3],
  // Largest grain the decoder adds at each brightness level for luma, Cb and Cr,
  // with the templates of the entry's own seed.
  pub peak: [[f64; 256]; 3],
}

// Grain deviation the decoder produces, on the 0 to 255 scale.
pub fn entry_stats(start: u64, end: u64, seed: u16, params: &GrainParams) -> EntryStats {
  let chroma_active = [
    params.chroma_scaling_from_luma || !params.scaling_points_cb.is_empty(),
    params.chroma_scaling_from_luma || !params.scaling_points_cr.is_empty(),
  ];
  let template = simulate(params, DEPTH, true, true, chroma_active);
  let extremes = extremes(params, seed, DEPTH, true, true);
  let step = (1u32 << (DEPTH - 8)) as f64;
  let unit = (1u32 << params.scaling_shift) as f64 * step;
  let scaling = scaling_per_level(params, chroma_active);
  let spread = [template.luma_std, template.chroma_std[0], template.chroma_std[1]];
  let round2 = |value: i32| {
    let shift = params.scaling_shift as u32;
    (value + (1 << (shift - 1))) >> shift
  };
  let mut deviation = [[0.0; 256]; 3];
  let mut peak = [[0.0; 256]; 3];
  for plane in 0..3 {
    for level in 0..256 {
      let scale = scaling[plane][level];
      deviation[plane][level] = scale as f64 * spread[plane] / unit;
      let [low, high] = extremes[plane];
      peak[plane][level] = round2(scale * low).abs().max(round2(scale * high).abs()) as f64 / step;
    }
  }
  let chroma = |index: usize, points: &[[u8; 2]]| -> f64 {
    if !chroma_active[index] {
      return 0.0;
    }
    let lut = if params.chroma_scaling_from_luma {
      scaling_lut(&params.scaling_points_y)
    } else {
      scaling_lut(points)
    };
    lut.iter().sum::<i32>() as f64 / 256.0 * template.chroma_std[index] / unit
  };
  EntryStats {
    start: start as f64 / TICKS_PER_SECOND as f64,
    end: (end < OPEN_END).then(|| end as f64 / TICKS_PER_SECOND as f64),
    seed,
    luma: [deviation[0][32], deviation[0][128], deviation[0][224]],
    luma_mean: deviation[0][16..236].iter().sum::<f64>() / 220.0,
    chroma: [chroma(0, &params.scaling_points_cb), chroma(1, &params.scaling_points_cr)],
    lag: params.ar_coeff_lag,
    correlation: (template.luma_rho[0] + template.luma_rho[1]) / 2.0,
    deviation,
    peak,
  }
}

pub fn table_stats(table: &GrainTable) -> Vec<EntryStats> {
  table
    .segments
    .iter()
    .map(|segment| entry_stats(segment.start, segment.end, segment.seed, &segment.params))
    .collect()
}

pub fn clock(seconds: f64) -> String {
  let whole = seconds.max(0.0);
  let minutes = (whole / 60.0).floor();
  let rest = whole - minutes * 60.0;
  if minutes >= 60.0 {
    format!("{}:{:02}:{:06.3}", (minutes / 60.0).floor(), minutes as u64 % 60, rest)
  } else {
    format!("{}:{:06.3}", minutes, rest)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn builds_the_scaling_function() {
    let lut = scaling_lut(&[[16, 20], [116, 40]]);
    assert_eq!(lut[0], 20);
    assert_eq!(lut[66], 30);
    assert_eq!(lut[255], 40);
    assert_eq!(scaling_lut(&[])[100], 0);
    // The decoder rounds each step, so 3 levels into a rise of 1 per 7 levels is 0.
    let lut = scaling_lut(&[[0, 0], [7, 1], [255, 1]]);
    assert_eq!(&lut[..8], &[0, 0, 0, 0, 1, 1, 1, 1]);
    // 10 bit samples between two levels take a rounded share of the step.
    let lut = scaling_lut(&[[0, 0], [1, 40], [255, 40]]);
    assert_eq!([0, 1, 2, 3, 4].map(|index| scale_at(&lut, index)), [0, 10, 20, 30, 40]);
  }

  #[test]
  fn chroma_scaling_follows_luma_and_offset() {
    let mut params = GrainParams {
      scaling_points_y: vec![[0, 10], [255, 200]],
      scaling_points_cb: vec![[0, 0], [255, 255]],
      scaling_points_cr: vec![[0, 50]],
      ..GrainParams::default()
    };
    // Default multipliers give index = luma, so the Cb curve rises with brightness.
    let scaling = scaling_per_level(&params, [true, true]);
    assert_eq!(scaling[1][100], 100);
    assert_eq!(scaling[2][100], 50);
    // A luma multiplier of 128 ignores luma, an offset moves the index.
    params.cb_luma_mult = 128;
    params.cb_mult = 192;
    params.cb_offset = 200;
    let scaling = scaling_per_level(&params, [true, true]);
    assert_eq!(scaling[1][0], 128 - 56);
    assert_eq!(scaling[1][255], 128 - 56);
    params.chroma_scaling_from_luma = true;
    let scaling = scaling_per_level(&params, [true, true]);
    assert_eq!(scaling[1], scaling[0]);
    assert_eq!(scaling_per_level(&params, [false, false])[2][100], 0);
  }

  #[test]
  fn reports_the_strength_a_decoder_produces() {
    let params = GrainParams {
      scaling_points_y: vec![[0, 32]],
      scaling_shift: 11,
      ..GrainParams::default()
    };
    let stats = entry_stats(0, 20_000_000, 7391, &params);
    // A white template in 10 bit has a deviation of 128, so 32 / 2048 * 128 / 4 is 0.5.
    assert!((stats.luma[1] - 0.5).abs() < 0.02, "{}", stats.luma[1]);
    assert_eq!(stats.deviation[0][128], stats.luma[1]);
    assert_eq!(stats.end, Some(2.0));
    assert_eq!(stats.chroma, [0.0, 0.0]);
    assert_eq!(stats.deviation[1], [0.0; 256]);
    assert_eq!(stats.peak[2], [0.0; 256]);
    // A Gaussian peak in a few thousand samples lies near 3.5 deviations.
    let ratio = stats.peak[0][128] / stats.deviation[0][128];
    assert!((3.0..4.5).contains(&ratio), "{ratio}");
    assert_eq!(clock(3725.5), "1:02:05.500");
    assert_eq!(clock(65.25), "1:05.250");
  }
}
