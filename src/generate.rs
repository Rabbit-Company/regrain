use crate::diff::model::ar_for_correlation;
use crate::diff::synth::{TemplateStats, simulate};
use crate::grain::{GrainParams, MAX_LUMA_POINTS};
use crate::report::fail;
use crate::table::{GrainTable, OPEN_END, Segment};
use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Size {
  /// Every pixel on its own, like digital camera noise
  Fine,
  /// Neighbouring pixels alike, like most film grain
  Medium,
  /// Clumps a few pixels wide, like grain from a fast film stock
  Coarse,
}

impl Size {
  // How alike neighbouring grain pixels are, as `stats` reports it.
  fn correlation(self) -> f64 {
    match self {
      Size::Fine => 0.0,
      Size::Medium => 0.35,
      Size::Coarse => 0.6,
    }
  }
}

pub enum Model {
  // Grain deviation on the 0 to 255 scale, weaker in deep shadows and highlights.
  Strength(f64),
  // Sensor noise of a full frame camera at this ISO and picture size.
  Iso { iso: u32, width: u32, height: u32 },
}

pub struct GenerateOptions {
  pub model: Model,
  pub size: Size,
  pub chroma: f64,
}

const LAG: u8 = 3;
const DEPTH: u8 = 10;

// Grain deviation on the 0 to 255 scale at limited range luma levels.
fn luma_targets(model: &Model) -> Vec<(u8, f64)> {
  match *model {
    Model::Strength(strength) => vec![(16, 0.6 * strength), (64, strength), (192, strength), (235, 0.7 * strength)],
    Model::Iso { iso, width, height } => photon_noise(iso, width, height),
  }
}

fn to_linear(value: f64) -> f64 {
  if value < 0.081 {
    value / 4.5
  } else {
    ((value + 0.099) / 1.099).powf(1.0 / 0.45)
  }
}

fn from_linear(linear: f64) -> f64 {
  if linear < 0.018 {
    linear * 4.5
  } else {
    1.099 * linear.powf(0.45) - 0.099
  }
}

// Shot, read and pixel response noise of a 36 x 24 mm sensor, with the constants of
// libaom's photon noise model, carried through the BT.709 transfer function.
fn photon_noise(iso: u32, width: u32, height: u32) -> Vec<(u8, f64)> {
  const PHOTONS_PER_LX_S_PER_UM2: f64 = 11260.0;
  const QUANTUM_EFFICIENCY: f64 = 0.20;
  const RESPONSE_NONUNIFORMITY: f64 = 0.005;
  const READ_NOISE: f64 = 1.5;
  const MID_TONE: f64 = 0.18;
  let exposure = 10.0 / iso as f64;
  let pixel_area = 36000.0 * 24000.0 / (width as f64 * height as f64);
  let mid_electrons = QUANTUM_EFFICIENCY * PHOTONS_PER_LX_S_PER_UM2 * exposure * pixel_area;
  let max_electrons = mid_electrons / MID_TONE;
  (0..MAX_LUMA_POINTS)
    .map(|point| {
      let encoded = point as f64 / (MAX_LUMA_POINTS - 1) as f64;
      let linear = to_linear(encoded);
      let electrons = max_electrons * linear;
      let noise = (READ_NOISE * READ_NOISE + electrons + (RESPONSE_NONUNIFORMITY * electrons).powi(2)).sqrt();
      let linear_noise = noise / max_electrons;
      let low = (linear - 2.0 * linear_noise).max(0.0);
      let high = (linear + 2.0 * linear_noise).min(2.0);
      let slope = (from_linear(high) - from_linear(low)) / (high - low);
      let level = (16.0 + 219.0 * encoded).round() as u8;
      (level, linear_noise * slope * 219.0)
    })
    .collect()
}

// Autoregression for round grain clumps: the coefficients are fitted to a correlation
// that falls off exponentially with distance, the same way in every direction, and the width of that
// fall-off is searched until the template has the neighbour correlation of the size.
// Weighting the causal taps directly would smear the clumps along a diagonal.
fn ar_coefficients(size: Size) -> (Vec<f64>, u8) {
  let target = size.correlation();
  if target == 0.0 {
    return (Vec::new(), 0);
  }
  let fit = |width: f64| ar_for_correlation(|dx, dy| (-((dx * dx + dy * dy) as f64).sqrt() / width).exp(), LAG as i32);
  let (mut low, mut high) = (0.2, 6.0);
  let mut coeffs = Vec::new();
  for _ in 0..24 {
    let width = (low + high) / 2.0;
    coeffs = fit(width);
    let template = template_for(&coeffs, LAG, [false, false]);
    if (template.luma_rho[0] + template.luma_rho[1]) / 2.0 < target {
      low = width;
    } else {
      high = width;
    }
  }
  (coeffs, LAG)
}

fn quantize(coeffs: &[f64]) -> (Vec<i8>, u8) {
  let largest = coeffs.iter().fold(0.0f64, |acc, &c| acc.max(c.abs()));
  let shift = (6..=9u8)
    .rev()
    .find(|&shift| largest * (1u32 << shift) as f64 <= 127.0)
    .unwrap_or(6);
  let values = coeffs
    .iter()
    .map(|&c| (c * (1u32 << shift) as f64).round().clamp(-128.0, 127.0) as i8)
    .collect();
  (values, shift)
}

fn base_params(coeffs: &[f64], lag: u8, chroma: [bool; 2]) -> GrainParams {
  let (values, shift) = quantize(coeffs);
  let count = 2 * lag as usize * (lag as usize + 1);
  let mut luma = values.clone();
  luma.resize(count, 0);
  let mut chroma_coeffs = values;
  chroma_coeffs.resize(count + 1, 0);
  GrainParams {
    ar_coeff_lag: lag,
    ar_coeff_shift: shift,
    scaling_points_y: vec![[0, 1]],
    scaling_points_cb: if chroma[0] { vec![[0, 1]] } else { Vec::new() },
    scaling_points_cr: if chroma[1] { vec![[0, 1]] } else { Vec::new() },
    ar_coeffs_y: luma,
    ar_coeffs_cb: chroma_coeffs.clone(),
    ar_coeffs_cr: chroma_coeffs,
    ..GrainParams::default()
  }
}

fn template_for(coeffs: &[f64], lag: u8, chroma: [bool; 2]) -> TemplateStats {
  simulate(&base_params(coeffs, lag, chroma), DEPTH, true, true, chroma)
}

pub fn generate(options: &GenerateOptions) -> Result<GrainTable> {
  if options.chroma < 0.0 || !options.chroma.is_finite() {
    return Err(fail("invalid_argument", "--chroma must be 0 or more"));
  }
  let targets = luma_targets(&options.model);
  if targets.iter().any(|&(_, deviation)| !deviation.is_finite() || deviation < 0.0) {
    return Err(fail("invalid_argument", "the grain strength must be 0 or more"));
  }
  let chroma_active = [options.chroma > 0.0; 2];
  let (coeffs, lag) = ar_coefficients(options.size);
  let mut params = base_params(&coeffs, lag, chroma_active);
  let template = simulate(&params, DEPTH, true, true, chroma_active);

  // A scaling value s gives a deviation of s * template / (2^shift * 4) on the 0 to 255
  // scale in 10 bit, the same at every bit depth.
  let peak = targets
    .iter()
    .map(|&(_, deviation)| deviation / template.luma_std)
    .chain(
      chroma_active
        .iter()
        .zip(template.chroma_std)
        .map(|(&on, std)| if on { options.chroma / std } else { 0.0 }),
    )
    .fold(0.0f64, f64::max);
  let Some(shift) = (8..=11u8).rev().find(|&shift| peak * (1u32 << shift) as f64 * 4.0 <= 255.0) else {
    let most = 255.0 * template.luma_std / 1024.0;
    return Err(fail(
      "invalid_argument",
      format!("that grain is stronger than AV1 can describe, the most at this size is about {most:.1}"),
    ));
  };
  let unit = (1u32 << shift) as f64 * 4.0;
  let value = |deviation: f64, std: f64| (deviation * unit / std).round().clamp(0.0, 255.0) as u8;
  params.scaling_shift = shift;
  params.scaling_points_y = targets
    .iter()
    .map(|&(level, deviation)| [level, value(deviation, template.luma_std)])
    .collect();
  for (index, points) in [&mut params.scaling_points_cb, &mut params.scaling_points_cr]
    .into_iter()
    .enumerate()
  {
    if chroma_active[index] {
      *points = vec![[0, value(options.chroma, template.chroma_std[index])]];
    }
  }
  for points in [
    &mut params.scaling_points_y,
    &mut params.scaling_points_cb,
    &mut params.scaling_points_cr,
  ] {
    if points.iter().all(|point| point[1] == 0) {
      points.clear();
    }
  }
  if params.scaling_points_y.is_empty() && params.scaling_points_cb.is_empty() && params.scaling_points_cr.is_empty() {
    return Ok(GrainTable::default());
  }
  params.validate().map_err(|error| fail("error", format!("{error:#}")))?;
  Ok(GrainTable {
    segments: vec![Segment {
      start: 0,
      end: OPEN_END,
      seed: 7391,
      params,
    }],
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::stats::entry_stats;

  fn single(options: &GenerateOptions) -> GrainParams {
    generate(options).unwrap().segments.remove(0).params
  }

  #[test]
  fn reaches_the_requested_strength() {
    for size in [Size::Fine, Size::Medium, Size::Coarse] {
      let params = single(&GenerateOptions {
        model: Model::Strength(1.5),
        size,
        chroma: 0.4,
      });
      let stats = entry_stats(0, OPEN_END, &params);
      assert!((stats.luma[1] - 1.5).abs() < 0.03, "{size:?} {}", stats.luma[1]);
      assert!((stats.chroma[0] - 0.4).abs() < 0.03, "{size:?} {:?}", stats.chroma);
      assert!(
        (stats.correlation - size.correlation()).abs() < 0.05,
        "{size:?} {}",
        stats.correlation
      );
    }
  }

  #[test]
  fn photon_noise_grows_with_iso() {
    let at = |iso| {
      let params = single(&GenerateOptions {
        model: Model::Iso {
          iso,
          width: 1920,
          height: 1080,
        },
        size: Size::Fine,
        chroma: 0.0,
      });
      entry_stats(0, OPEN_END, &params)
    };
    let (low, high) = (at(400), at(3200));
    // Shot noise grows with the square root of the ISO, 2.8 times for 8 times the ISO.
    // Pixel response non-uniformity does not, and takes a large share at ISO 400.
    let ratio = high.luma[1] / low.luma[1];
    assert!(ratio > 1.9 && ratio < 2.6, "{ratio}");
    assert_eq!(high.chroma, [0.0, 0.0]);
  }

  #[test]
  fn refuses_grain_beyond_av1() {
    let error = generate(&GenerateOptions {
      model: Model::Strength(80.0),
      size: Size::Fine,
      chroma: 0.0,
    })
    .unwrap_err();
    assert_eq!(crate::report::error_code(&error), "invalid_argument");
    let empty = generate(&GenerateOptions {
      model: Model::Strength(0.0),
      size: Size::Fine,
      chroma: 0.0,
    })
    .unwrap();
    assert!(empty.segments.is_empty());
  }
}
