use crate::diff::synth::simulate;
use crate::grain::GrainParams;
use crate::table::{GrainTable, OPEN_END, TICKS_PER_SECOND};

// Scaling function values at each of the 256 brightness levels, as an AV1 decoder
// builds them from the points.
fn scaling_lut(points: &[[u8; 2]]) -> [f64; 256] {
  let mut lut = [0.0; 256];
  let Some(first) = points.first() else {
    return lut;
  };
  let last = points[points.len() - 1];
  for (level, value) in lut.iter_mut().enumerate() {
    *value = if level <= first[0] as usize {
      first[1] as f64
    } else if level >= last[0] as usize {
      last[1] as f64
    } else {
      let pair = points.windows(2).find(|pair| level <= pair[1][0] as usize).unwrap();
      let t = (level - pair[0][0] as usize) as f64 / (pair[1][0] - pair[0][0]) as f64;
      pair[0][1] as f64 * (1.0 - t) + pair[1][1] as f64 * t
    };
  }
  lut
}

pub struct EntryStats {
  pub start: f64,
  pub end: Option<f64>,
  pub luma: [f64; 3],
  pub luma_mean: f64,
  pub chroma: [f64; 2],
  pub lag: u8,
  pub correlation: f64,
}

// Grain deviation the decoder produces, on the 0 to 255 scale.
pub fn entry_stats(start: u64, end: u64, params: &GrainParams) -> EntryStats {
  let chroma_active = [
    params.chroma_scaling_from_luma || !params.scaling_points_cb.is_empty(),
    params.chroma_scaling_from_luma || !params.scaling_points_cr.is_empty(),
  ];
  let template = simulate(params, 10, true, true, chroma_active);
  let unit = (1u32 << params.scaling_shift) as f64 * 4.0;
  let luma_lut = scaling_lut(&params.scaling_points_y);
  let luma = |level: usize| luma_lut[level] * template.luma_std / unit;
  let chroma = |index: usize, points: &[[u8; 2]]| -> f64 {
    if !chroma_active[index] {
      return 0.0;
    }
    let lut = if params.chroma_scaling_from_luma {
      luma_lut
    } else {
      scaling_lut(points)
    };
    lut.iter().sum::<f64>() / 256.0 * template.chroma_std[index] / unit
  };
  EntryStats {
    start: start as f64 / TICKS_PER_SECOND as f64,
    end: (end < OPEN_END).then(|| end as f64 / TICKS_PER_SECOND as f64),
    luma: [luma(32), luma(128), luma(224)],
    luma_mean: (16..236).map(luma).sum::<f64>() / 220.0,
    chroma: [chroma(0, &params.scaling_points_cb), chroma(1, &params.scaling_points_cr)],
    lag: params.ar_coeff_lag,
    correlation: (template.luma_rho[0] + template.luma_rho[1]) / 2.0,
  }
}

pub fn table_stats(table: &GrainTable) -> Vec<EntryStats> {
  table
    .segments
    .iter()
    .map(|segment| entry_stats(segment.start, segment.end, &segment.params))
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
    assert_eq!(lut[0], 20.0);
    assert_eq!(lut[66], 30.0);
    assert_eq!(lut[255], 40.0);
    assert_eq!(scaling_lut(&[])[100], 0.0);
  }

  #[test]
  fn reports_the_strength_a_decoder_produces() {
    let params = GrainParams {
      scaling_points_y: vec![[0, 32]],
      scaling_shift: 11,
      ..GrainParams::default()
    };
    let stats = entry_stats(0, 20_000_000, &params);
    // A white template in 10 bit has a deviation of 128, so 32 / 2048 * 128 / 4 is 0.5.
    assert!((stats.luma[1] - 0.5).abs() < 0.02, "{}", stats.luma[1]);
    assert_eq!(stats.end, Some(2.0));
    assert_eq!(stats.chroma, [0.0, 0.0]);
    assert_eq!(clock(3725.5), "1:02:05.500");
    assert_eq!(clock(65.25), "1:05.250");
  }
}
