use super::analyze::{BINS, BUCKETS, CROSS, LAGS, PlaneStats, ar_offsets, bucket_variance, lag_index};
use super::probe::PixelFormat;
use super::synth::simulate;
use crate::grain::{GrainParams, MAX_CHROMA_POINTS, MAX_LUMA_POINTS};

const MIN_BIN_BLOCKS: u64 = 16;
const SMOOTHING_BLOCKS: f64 = 150.0;
const MEAN_CAP: f64 = 8.0;
const MIN_STRICT_BLOCKS: u64 = 100;
const MIN_CORRELATION_WEIGHT: f64 = 24.0;
const MIN_NATIVE_VARIANCE: f64 = 0.02;
const MIN_NATIVE_SIGMA: f64 = 0.25;
const LAG_TOLERANCE: f64 = 1.004;
const POINT_TOLERANCE: f64 = 0.4;

#[derive(Clone, Debug, Default)]
// Strictly flat blocks of every plane, followed by the loosely flat ones.
pub struct RunStats {
  pub planes: Vec<PlaneStats>,
}

impl RunStats {
  pub fn new(planes: usize) -> Self {
    Self {
      planes: vec![PlaneStats::default(); planes * 2],
    }
  }

  // The statistics a fit uses: strictly flat blocks, or loosely flat ones as well when a
  // plane has too few strict ones.
  pub fn effective(&self) -> Vec<PlaneStats> {
    let count = self.planes.len() / 2;
    (0..count)
      .map(|plane| {
        let mut stats = self.planes[plane].clone();
        if stats.blocks < MIN_STRICT_BLOCKS {
          stats.merge(&self.planes[plane + count]);
        }
        stats
      })
      .collect()
  }

  pub fn merge(&mut self, other: &RunStats) {
    for (a, b) in self.planes.iter_mut().zip(&other.planes) {
      a.merge(b);
    }
  }

  pub fn luma_blocks(&self) -> u64 {
    self.planes[0].blocks + self.planes[self.planes.len() / 2].blocks
  }
}

fn erf(x: f64) -> f64 {
  let t = 1.0 / (1.0 + 0.3275911 * x.abs());
  let poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
  let value = 1.0 - poly * (-x * x).exp();
  if x >= 0.0 { value } else { -value }
}

// Variance of a zero mean Gaussian after rounding to whole code values.
fn rounded_variance(sigma: f64) -> f64 {
  if sigma <= 0.0 {
    return 0.0;
  }
  if sigma > 4.0 {
    return sigma * sigma + 1.0 / 12.0;
  }
  let scale = 1.0 / (sigma * std::f64::consts::SQRT_2);
  let mut total = 0.0;
  for k in 1..=24 {
    let k = k as f64;
    total += k * k * (erf((k + 0.5) * scale) - erf((k - 0.5) * scale));
  }
  total
}

// The decoder rounds the grain it adds to whole code values of the encode, which adds
// variance of its own. `step` is the size of one such code value in measured units.
fn sigma_before_rounding(variance: f64, step: f64) -> f64 {
  if variance < MIN_NATIVE_VARIANCE {
    return 0.0;
  }
  let (mut low, mut high) = (0.0, variance.sqrt() + step);
  for _ in 0..48 {
    let mid = (low + high) / 2.0;
    if step * step * rounded_variance(mid / step) < variance {
      low = mid
    } else {
      high = mid
    }
  }
  (low + high) / 2.0
}

// The mean block variance follows grain whose strength varies within a brightness
// level. Blocks far above the median are almost always detail the denoiser removed,
// so they are left out.
fn capped_mean(histogram: &[u32], total: u64) -> f64 {
  let half = total as f64 / 2.0;
  let mut below = 0.0;
  let mut median = 0.0;
  for (bucket, &count) in histogram.iter().enumerate() {
    let count = count as f64;
    if count > 0.0 && below + count >= half {
      median = bucket_variance(bucket as f64 + (half - below) / count);
      break;
    }
    below += count;
  }
  let cap = median * MEAN_CAP;
  let mut sum = 0.0;
  let mut used = 0.0;
  for (bucket, &count) in histogram.iter().enumerate() {
    let value = bucket_variance(bucket as f64 + 0.5);
    if count == 0 || (bucket > 0 && value > cap) {
      continue;
    }
    sum += count as f64 * value;
    used += count as f64;
  }
  if used > 0.0 { sum / used } else { median }
}

#[derive(Clone, Debug)]
pub struct Curve {
  // Grain deviation per brightness bin, in native code values.
  pub sigma: [f64; BINS],
  pub counts: [u64; BINS],
  pub active: bool,
}

impl Curve {
  pub fn measure(stats: &PlaneStats, bit_depth: u8, output_depth: u8) -> Curve {
    let step = (bit_depth as f64 - output_depth as f64).exp2();
    let native = (1u64 << (2 * (bit_depth - 8))) as f64;
    let mut curve = Curve {
      sigma: [0.0; BINS],
      counts: [0; BINS],
      active: false,
    };
    let mut valid = [false; BINS];
    for bin in 0..BINS {
      let histogram = &stats.histogram[bin * BUCKETS..(bin + 1) * BUCKETS];
      let total: u64 = histogram.iter().map(|&count| count as u64).sum();
      curve.counts[bin] = total;
      if total < MIN_BIN_BLOCKS {
        continue;
      }
      curve.sigma[bin] = sigma_before_rounding(capped_mean(histogram, total) * native, step);
      valid[bin] = true;
    }
    let known: Vec<usize> = (0..BINS).filter(|&bin| valid[bin]).collect();
    if known.is_empty() {
      return curve;
    }
    // Bins with few blocks lean on their neighbours, bins with many keep their own value.
    let raw = curve.sigma;
    for &bin in &known {
      let mut total = 0.0;
      let mut weight = 0.0;
      for &other in &known {
        let distance = bin.abs_diff(other);
        if distance <= 3 {
          let w = (4 - distance) as f64 * curve.counts[other] as f64;
          total += w * raw[other];
          weight += w;
        }
      }
      let own = curve.counts[bin] as f64 / (curve.counts[bin] as f64 + SMOOTHING_BLOCKS);
      curve.sigma[bin] = own * raw[bin] + (1.0 - own) * total / weight;
    }
    for bin in 0..BINS {
      if valid[bin] {
        continue;
      }
      let after = known.iter().copied().find(|&k| k > bin);
      let before = known.iter().copied().rev().find(|&k| k < bin);
      curve.sigma[bin] = match (before, after) {
        (Some(a), Some(b)) => {
          let t = (bin - a) as f64 / (b - a) as f64;
          curve.sigma[a] * (1.0 - t) + curve.sigma[b] * t
        }
        (Some(a), None) => curve.sigma[a],
        (None, Some(b)) => curve.sigma[b],
        (None, None) => 0.0,
      };
    }
    curve.active = known.iter().any(|&bin| curve.sigma[bin] >= MIN_NATIVE_SIGMA);
    curve
  }

  pub fn distance(&self, other: &Curve) -> f64 {
    if !self.active && !other.active {
      return 0.0;
    }
    if self.active != other.active {
      return 1.0;
    }
    let mut difference = 0.0;
    let mut total = 0.0;
    for bin in 0..BINS {
      let weight = (self.counts[bin].min(other.counts[bin]) as f64).sqrt() + 1e-3;
      difference += weight * (self.sigma[bin] - other.sigma[bin]).abs();
      total += weight * self.sigma[bin].max(other.sigma[bin]).max(MIN_NATIVE_SIGMA);
    }
    difference / total
  }

  pub fn mean(&self) -> f64 {
    let total: u64 = self.counts.iter().sum();
    if total == 0 {
      return 0.0;
    }
    (0..BINS).map(|bin| self.sigma[bin] * self.counts[bin] as f64).sum::<f64>() / total as f64
  }
}

fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
  let n = b.len();
  for col in 0..n {
    let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
    if a[pivot][col].abs() < 1e-12 {
      return None;
    }
    a.swap(col, pivot);
    b.swap(col, pivot);
    for row in col + 1..n {
      let factor = a[row][col] / a[col][col];
      if factor == 0.0 {
        continue;
      }
      for k in col..n {
        a[row][k] -= factor * a[col][k];
      }
      b[row] -= factor * b[col];
    }
  }
  let mut x = vec![0.0; n];
  for row in (0..n).rev() {
    let mut value = b[row];
    for k in row + 1..n {
      value -= a[row][k] * x[k];
    }
    x[row] = value / a[row][row];
  }
  Some(x)
}

#[derive(Clone, Debug)]
struct ArFit {
  coeffs: Vec<f64>,
  luma_coeff: f64,
  error: f64,
}

fn fit_ar(stats: &PlaneStats, lag: i32, with_luma: bool) -> ArFit {
  let white = ArFit {
    coeffs: vec![0.0; ar_offsets(lag).len()],
    luma_coeff: 0.0,
    error: 1.0,
  };
  if stats.weight < MIN_CORRELATION_WEIGHT {
    return white;
  }
  let rho: Vec<f64> = (0..LAGS).map(|i| stats.correlation[i] / stats.weight).collect();
  let offsets = ar_offsets(lag);
  let use_luma = with_luma && stats.cross_weight >= MIN_CORRELATION_WEIGHT;
  let n = offsets.len() + usize::from(use_luma);
  if n == 0 {
    return white;
  }
  let all_offsets = ar_offsets(3);
  let cross = |offset: (i32, i32)| -> f64 {
    let index = if offset == (0, 0) {
      CROSS - 1
    } else {
      all_offsets.iter().position(|&o| o == offset).unwrap()
    };
    stats.cross[index] / stats.cross_weight
  };
  let mut a = vec![vec![0.0; n]; n];
  let mut b = vec![0.0; n];
  for (i, &(xi, yi)) in offsets.iter().enumerate() {
    for (j, &(xj, yj)) in offsets.iter().enumerate() {
      a[i][j] = rho[lag_index(xi - xj, yi - yj)];
    }
    a[i][i] += 1e-4;
    b[i] = rho[lag_index(xi, yi)];
    if use_luma {
      a[i][n - 1] = cross((xi, yi));
      a[n - 1][i] = a[i][n - 1];
    }
  }
  if use_luma {
    a[n - 1][n - 1] = stats.luma_power / stats.cross_weight + 1e-4;
    b[n - 1] = cross((0, 0));
  }
  let Some(solution) = solve(a, b.clone()) else {
    return white;
  };
  let error = 1.0 - solution.iter().zip(&b).map(|(x, y)| x * y).sum::<f64>();
  if !error.is_finite() || error <= 0.01 || error > 1.0 + 1e-6 {
    return white;
  }
  let luma_coeff = if use_luma { solution[n - 1] } else { 0.0 };
  ArFit {
    coeffs: solution[..offsets.len()].to_vec(),
    luma_coeff,
    error: error.min(1.0),
  }
}

fn choose_lag(stats: &PlaneStats, with_luma: bool) -> i32 {
  let best = fit_ar(stats, 3, with_luma).error;
  (0..3)
    .find(|&lag| fit_ar(stats, lag, with_luma).error <= best * LAG_TOLERANCE)
    .unwrap_or(3)
}

// AV1 holds the first and last scaling values for every level beyond them. Where the
// source has no grain at such a level, such as letterbox bars, the grain is cut to zero
// there. The cut reaches one level further in, since encoders often miss flat areas by
// one level next to the picture, and the grain comes back in full one level after that.
fn scaling_points(curve: &Curve, scale: f64, max_points: usize, constant: &[u32]) -> Vec<[u8; 2]> {
  let mut points: Vec<(f64, f64)> = (0..BINS)
    .filter(|&bin| curve.counts[bin] >= MIN_BIN_BLOCKS)
    .map(|bin| ((bin * 256 / BINS + 128 / BINS) as f64, (curve.sigma[bin] * scale).min(255.0)))
    .collect();
  let (first, last) = (points[0].0 as usize, points[points.len() - 1].0 as usize);
  let blank = |level: &usize| constant[*level] as u64 >= MIN_BIN_BLOCKS;
  let below = (0..first).rev().find(blank).map(|level| (level + 1).min(first - 1));
  let above = (last + 1..256).find(blank).map(|level| (level - 1).max(last + 1));
  let reserved = below.map_or(0, |level| 1 + usize::from(level + 1 < first)) + above.map_or(0, |level| 1 + usize::from(level - 1 > last));
  let max_points = max_points.saturating_sub(reserved).max(1);
  let reference = points.clone();
  let evaluate = |points: &[(f64, f64)], x: f64| -> f64 {
    if x <= points[0].0 {
      return points[0].1;
    }
    for pair in points.windows(2) {
      if x <= pair[1].0 {
        let t = (x - pair[0].0) / (pair[1].0 - pair[0].0);
        return pair[0].1 * (1.0 - t) + pair[1].1 * t;
      }
    }
    points[points.len() - 1].1
  };
  while points.len() > 1 {
    let mut best: Option<(usize, f64)> = None;
    for remove in 0..points.len() {
      let mut trial = points.clone();
      trial.remove(remove);
      let error = reference.iter().map(|&(x, y)| (evaluate(&trial, x) - y).abs()).fold(0.0, f64::max);
      if best.is_none_or(|(_, e)| error < e) {
        best = Some((remove, error));
      }
    }
    let (remove, error) = best.unwrap();
    if points.len() <= max_points && error > POINT_TOLERANCE {
      break;
    }
    points.remove(remove);
  }
  if let Some(level) = below {
    let edge = (level as f64 + 1.0, points[0].1);
    if edge.0 < points[0].0 {
      points.insert(0, edge);
    }
    points.insert(0, (level as f64, 0.0));
  }
  if let Some(level) = above {
    let edge = (level as f64 - 1.0, points[points.len() - 1].1);
    if edge.0 > points[points.len() - 1].0 {
      points.push(edge);
    }
    points.push((level as f64, 0.0));
  }
  points.iter().map(|&(x, y)| [x as u8, y.round().clamp(0.0, 255.0) as u8]).collect()
}

#[derive(Clone, Debug)]
pub struct Fit {
  pub curves: Vec<Curve>,
  pub rho: [f64; 2],
  pub params: Option<GrainParams>,
}

impl Fit {
  pub fn similar(&self, other: &Fit, tolerance: f64) -> bool {
    self.curves.iter().zip(&other.curves).all(|(a, b)| a.distance(b) <= tolerance)
      && (self.rho[0] - other.rho[0]).abs() <= tolerance
      && (self.rho[1] - other.rho[1]).abs() <= tolerance
  }
}

pub fn fit(run: &RunStats, format: PixelFormat, output_depth: u8) -> Fit {
  let planes = run.effective();
  let bit_depth = format.bit_depth;
  let curves: Vec<Curve> = planes.iter().map(|plane| Curve::measure(plane, bit_depth, output_depth)).collect();
  let luma = &planes[0];
  let rho = if luma.weight >= MIN_CORRELATION_WEIGHT {
    [
      luma.correlation[lag_index(1, 0)] / luma.weight,
      luma.correlation[lag_index(0, 1)] / luma.weight,
    ]
  } else {
    [0.0, 0.0]
  };
  let luma_active = curves[0].active;
  let lumaless_420 = format.subsampling_x && format.subsampling_y && !luma_active;
  let chroma_active: Vec<bool> = (1..curves.len()).map(|plane| curves[plane].active && !lumaless_420).collect();
  if !luma_active && !chroma_active.iter().any(|&active| active) {
    return Fit { curves, rho, params: None };
  }

  let mut lag = if luma_active { choose_lag(luma, false) } else { 0 };
  for (index, &active) in chroma_active.iter().enumerate() {
    if active {
      lag = lag.max(choose_lag(&planes[index + 1], luma_active));
    }
  }
  let luma_fit = if luma_active {
    fit_ar(luma, lag, false)
  } else {
    fit_ar(&PlaneStats::default(), lag, false)
  };
  let chroma_fits: Vec<ArFit> = chroma_active
    .iter()
    .enumerate()
    .map(|(index, &active)| {
      let mut fit = if active {
        fit_ar(&planes[index + 1], lag, luma_active)
      } else {
        fit_ar(&PlaneStats::default(), lag, false)
      };
      fit.luma_coeff *= (luma_fit.error / fit.error).sqrt();
      if fit.luma_coeff.abs() < 0.02 {
        fit.luma_coeff = 0.0;
      }
      fit
    })
    .collect();

  let mut damping = 1.0;
  let mut params = GrainParams::default();
  let mut template = Default::default();
  for _ in 0..12 {
    let largest = luma_fit
      .coeffs
      .iter()
      .chain(
        chroma_fits
          .iter()
          .flat_map(|fit| fit.coeffs.iter().chain(std::iter::once(&fit.luma_coeff))),
      )
      .fold(0.0f64, |acc, &c| acc.max((c * damping).abs()));
    let shift = (6..=9u8)
      .rev()
      .find(|&shift| largest * (1u32 << shift) as f64 <= 127.0)
      .unwrap_or(6);
    let quantize = |value: f64| (value * damping * (1u32 << shift) as f64).round().clamp(-128.0, 127.0) as i8;
    params = GrainParams {
      ar_coeff_lag: lag as u8,
      ar_coeff_shift: shift,
      ..GrainParams::default()
    };
    if luma_active {
      params.ar_coeffs_y = luma_fit.coeffs.iter().map(|&c| quantize(c)).collect();
      params.scaling_points_y = vec![[0, 1]];
    }
    let chroma_coeffs = |fit: &ArFit| -> Vec<i8> {
      fit
        .coeffs
        .iter()
        .map(|&c| quantize(c))
        .chain(std::iter::once(quantize(fit.luma_coeff)))
        .collect()
    };
    params.ar_coeffs_cb = chroma_fits
      .first()
      .map(chroma_coeffs)
      .unwrap_or_else(|| vec![0; luma_fit.coeffs.len() + 1]);
    params.ar_coeffs_cr = chroma_fits
      .get(1)
      .map(chroma_coeffs)
      .unwrap_or_else(|| vec![0; luma_fit.coeffs.len() + 1]);
    let active = [
      chroma_active.first().copied().unwrap_or(false),
      chroma_active.get(1).copied().unwrap_or(false),
    ];
    template = simulate(&params, bit_depth, format.subsampling_x, format.subsampling_y, active);
    let base = (512u32 >> (12 - bit_depth)) as f64;
    let limit = base * 6.0;
    if template.luma_std <= limit && template.chroma_std[0] <= limit && template.chroma_std[1] <= limit {
      break;
    }
    damping *= 0.9;
  }

  let stds = [template.luma_std, template.chroma_std[0], template.chroma_std[1]];
  let actives = [
    luma_active,
    chroma_active.first().copied().unwrap_or(false),
    chroma_active.get(1).copied().unwrap_or(false),
  ];
  let mut peak = 0.0f64;
  for plane in 0..curves.len() {
    if actives[plane] && stds[plane] > 0.0 {
      let highest = curves[plane].sigma.iter().copied().fold(0.0, f64::max);
      peak = peak.max(highest / stds[plane]);
    }
  }
  let scaling_shift = (8..=11u8).rev().find(|&shift| peak * (1u32 << shift) as f64 <= 255.0).unwrap_or(8);
  params.scaling_shift = scaling_shift;
  let unit = (1u32 << scaling_shift) as f64;
  params.scaling_points_y = if luma_active {
    scaling_points(&curves[0], unit / stds[0], MAX_LUMA_POINTS, &planes[0].constant)
  } else {
    Vec::new()
  };
  if actives[1] {
    params.scaling_points_cb = scaling_points(&curves[1], unit / stds[1], MAX_CHROMA_POINTS, &planes[1].constant);
  }
  if actives[2] {
    params.scaling_points_cr = scaling_points(&curves[2], unit / stds[2], MAX_CHROMA_POINTS, &planes[2].constant);
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
  if params.is_empty() {
    return Fit { curves, rho, params: None };
  }
  Fit {
    curves,
    rho,
    params: Some(params),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rounding_model_matches_known_values() {
    assert!((rounded_variance(10.0) - 100.0833).abs() < 0.01);
    assert!((rounded_variance(1.0) - 1.0833).abs() < 0.01);
    assert!(rounded_variance(0.2) < 0.02);
    for sigma in [0.4, 0.8, 1.5, 3.0, 9.0] {
      let recovered = sigma_before_rounding(rounded_variance(sigma), 1.0);
      assert!((recovered - sigma).abs() < 0.01, "{sigma} became {recovered}");
    }
    assert_eq!(sigma_before_rounding(0.001, 1.0), 0.0);
    // Measured in 8 bit and added in 10 bit, the rounding is a sixteenth as strong.
    let finer = sigma_before_rounding(1.0, 0.25);
    assert!((finer * finer - (1.0 - 1.0 / 192.0)).abs() < 0.01, "{finer}");
  }

  #[test]
  fn leaves_out_blocks_far_above_the_median() {
    let mut histogram = vec![0u32; BUCKETS];
    let typical = super::super::analyze::variance_bucket(1.0);
    histogram[typical] = 90;
    histogram[typical + 4] = 9;
    histogram[super::super::analyze::variance_bucket(400.0)] = 1;
    let mean = capped_mean(&histogram, 100);
    assert!(mean > 1.0 && mean < 1.2, "{mean}");
  }

  #[test]
  fn solves_linear_systems() {
    let x = solve(vec![vec![2.0, 1.0], vec![1.0, 3.0]], vec![5.0, 10.0]).unwrap();
    assert!((x[0] - 1.0).abs() < 1e-12 && (x[1] - 3.0).abs() < 1e-12);
    assert!(solve(vec![vec![1.0, 2.0], vec![2.0, 4.0]], vec![1.0, 2.0]).is_none());
  }

  #[test]
  fn reduces_points_to_the_limit() {
    let mut curve = Curve {
      sigma: [0.0; BINS],
      counts: [100; BINS],
      active: true,
    };
    for bin in 0..BINS {
      curve.sigma[bin] = 2.0 + ((bin as f64) * 0.9).sin();
    }
    let points = scaling_points(&curve, 16.0, MAX_LUMA_POINTS, &[0; 256]);
    assert!(points.len() <= MAX_LUMA_POINTS && points.len() >= 4);
    assert!(points.windows(2).all(|pair| pair[1][0] > pair[0][0]));
    let flat = Curve {
      sigma: [1.5; BINS],
      counts: [100; BINS],
      active: true,
    };
    let points = scaling_points(&flat, 16.0, MAX_LUMA_POINTS, &[0; 256]);
    assert_eq!(points.len(), 1);
    assert_eq!(points[0][1], 24);
  }

  #[test]
  fn cuts_grain_where_the_source_has_none() {
    let mut curve = Curve {
      sigma: [0.0; BINS],
      counts: [0; BINS],
      active: true,
    };
    for bin in 6..26 {
      curve.sigma[bin] = 2.0 + ((bin as f64) * 0.9).sin();
      curve.counts[bin] = 100;
    }
    let mut constant = [0u32; 256];
    constant[16] = 500;
    constant[128] = 500;
    constant[235] = 500;
    let points = scaling_points(&curve, 16.0, MAX_LUMA_POINTS, &constant);
    assert!(points.len() <= MAX_LUMA_POINTS, "{points:?}");
    assert!(points.windows(2).all(|pair| pair[1][0] > pair[0][0]), "{points:?}");
    assert_eq!(points[0], [17, 0]);
    assert_eq!(points[1][0], 18);
    assert_eq!(points[1][1], points[2][1]);
    let n = points.len();
    assert_eq!(points[n - 1], [234, 0]);
    assert_eq!(points[n - 2][0], 233);
    assert_eq!(points[n - 2][1], points[n - 3][1]);
    // A level inside the measured range keeps its measured grain.
    assert!(points[1..n - 1].iter().all(|point| point[1] > 0));

    constant[235] = MIN_BIN_BLOCKS as u32 - 1;
    let points = scaling_points(&curve, 16.0, MAX_LUMA_POINTS, &constant);
    assert_ne!(points[points.len() - 1][1], 0);
  }
}
