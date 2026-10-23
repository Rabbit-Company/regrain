use rabbit_av1::grain::GrainParams;

const SIZE: usize = 160;
const MARGIN: usize = 12;
const ROUNDS: u64 = 3;

struct Gaussian(u64);

impl Gaussian {
  fn uniform(&mut self) -> f64 {
    self.0 ^= self.0 << 13;
    self.0 ^= self.0 >> 7;
    self.0 ^= self.0 << 17;
    ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
  }

  fn next(&mut self) -> i32 {
    let radius = (-2.0 * self.uniform().ln()).sqrt();
    let value = radius * (std::f64::consts::TAU * self.uniform()).cos() * 512.0;
    (value.round() as i32).clamp(-2047, 2047)
  }
}

// The Gaussian values of every round, which are the same for every filter, so
// they are drawn once instead of for each entry a table describes. A round
// reads luma, then Cb, then Cr, each only when it has grain.
fn gaussian_rounds() -> &'static [Vec<i32>] {
  static ROUNDS_VALUES: std::sync::OnceLock<Vec<Vec<i32>>> = std::sync::OnceLock::new();
  ROUNDS_VALUES.get_or_init(|| {
    // 4:4:4 chroma planes are the largest a round can read.
    let chroma = ((SIZE - 4) + 3) * ((SIZE - 4) + 3);
    (0..ROUNDS)
      .map(|round| {
        let mut gaussian = Gaussian(0x9e37_79b9_7f4a_7c15 ^ (round + 1).wrapping_mul(0xd1b5_4a32_d192_ed03));
        (0..SIZE * SIZE + 2 * chroma).map(|_| gaussian.next()).collect()
      })
      .collect()
  })
}

fn round2(value: i32, shift: u32) -> i32 {
  if shift == 0 { value } else { (value + (1 << (shift - 1))) >> shift }
}

// Runs the autoregressive filter of section 7.18.3.3 over rows 3 and below of a
// grain plane, columns 3 to width - 3, adding `extra(x, y)` to each sum, as the
// chroma filter does with the luma grain. The rows above a row are final before
// it is filtered, so their share of every sum is added up first in loops that
// vectorize, and only the taps of the row itself run pixel by pixel. The sums
// are the same integers as with the taps in specification order.
#[allow(clippy::too_many_arguments)]
pub fn auto_regress(
  plane: &mut [i32],
  width: usize,
  height: usize,
  lag: i32,
  coeffs: &[i8],
  shift: u32,
  (min, max): (i32, i32),
  mut extra: impl FnMut(usize, usize) -> i32,
) {
  let lag = lag as usize;
  let coeff = |pos: usize| coeffs.get(pos).copied().unwrap_or(0) as i32;
  // Taps of the rows above as (row above, column offset + lag, coefficient), and
  // of the row itself, left of the pixel, as (column offset + lag, coefficient).
  let mut above = Vec::new();
  let mut left = Vec::new();
  let mut pos = 0;
  for dy in (1..=lag).rev() {
    for dx in 0..=2 * lag {
      if coeff(pos) != 0 {
        above.push((dy, dx, coeff(pos)));
      }
      pos += 1;
    }
  }
  for dx in 0..lag {
    if coeff(pos) != 0 {
      left.push((dx, coeff(pos)));
    }
    pos += 1;
  }
  let columns = width - 6;
  let mut sums = vec![0i32; columns];
  for y in 3..height {
    sums.fill(0);
    for &(dy, dx, c) in &above {
      let start = (y - dy) * width + 3 + dx - lag;
      for (sum, &value) in sums.iter_mut().zip(&plane[start..start + columns]) {
        *sum += c * value;
      }
    }
    let row = y * width;
    for (column, &sum) in sums.iter().enumerate() {
      let x = column + 3;
      let mut sum = sum + extra(x, y);
      for &(dx, c) in &left {
        sum += c * plane[row + x + dx - lag];
      }
      let index = row + x;
      plane[index] = (plane[index] + round2(sum, shift)).clamp(min, max);
    }
  }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TemplateStats {
  pub luma_std: f64,
  pub chroma_std: [f64; 2],
  pub luma_rho: [f64; 2],
}

fn deviation(grid: &[i32], width: usize, height: usize) -> (f64, f64, f64) {
  let mut sum = 0.0;
  let mut sum_sq = 0.0;
  let mut horizontal = 0.0;
  let mut vertical = 0.0;
  let mut count = 0.0;
  for y in MARGIN..height - 1 {
    for x in MARGIN..width - MARGIN {
      let value = grid[y * width + x] as f64;
      sum += value;
      sum_sq += value * value;
      horizontal += value * grid[y * width + x + 1] as f64;
      vertical += value * grid[(y + 1) * width + x] as f64;
      count += 1.0;
    }
  }
  let mean = sum / count;
  let variance = (sum_sq / count - mean * mean).max(0.0);
  if variance == 0.0 {
    return (0.0, 0.0, 0.0);
  }
  (
    variance,
    (horizontal / count - mean * mean) / variance,
    (vertical / count - mean * mean) / variance,
  )
}

// Builds grain templates the way an AV1 decoder does and measures them, so
// scaling values can be set for the grain the decoder will really produce.
pub fn simulate(params: &GrainParams, bit_depth: u8, subsampling_x: bool, subsampling_y: bool, chroma: [bool; 2]) -> TemplateStats {
  let lag = params.ar_coeff_lag as i32;
  let luma_count = params.luma_coeff_count();
  let gauss_shift = 12 - bit_depth as u32 + params.grain_scale_shift as u32;
  let ar_shift = params.ar_coeff_shift as u32;
  let centre = 128i32 << (bit_depth - 8);
  let (grain_min, grain_max) = (-centre, (256 << (bit_depth - 8)) - 1 - centre);
  let has_luma = !params.scaling_points_y.is_empty();
  let (sx, sy) = (usize::from(subsampling_x), usize::from(subsampling_y));
  let chroma_w = ((SIZE - 4 - sx) >> sx) + 3;
  let chroma_h = ((SIZE - 4 - sy) >> sy) + 3;
  let mut stats = TemplateStats::default();
  let mut luma = vec![0i32; SIZE * SIZE];
  let mut plane = vec![0i32; chroma_w * chroma_h];

  for values in gaussian_rounds() {
    let mut gaussian = values.iter();
    if has_luma {
      for (value, &raw) in luma.iter_mut().zip(gaussian.by_ref()) {
        *value = round2(raw, gauss_shift);
      }
      auto_regress(
        &mut luma,
        SIZE,
        SIZE,
        lag,
        &params.ar_coeffs_y,
        ar_shift,
        (grain_min, grain_max),
        |_, _| 0,
      );
      let (variance, horizontal, vertical) = deviation(&luma, SIZE, SIZE);
      stats.luma_std += variance / ROUNDS as f64;
      stats.luma_rho[0] += horizontal / ROUNDS as f64;
      stats.luma_rho[1] += vertical / ROUNDS as f64;
    }
    for (index, coeffs) in [&params.ar_coeffs_cb, &params.ar_coeffs_cr].into_iter().enumerate() {
      if !chroma[index] {
        continue;
      }
      for (value, &raw) in plane.iter_mut().zip(gaussian.by_ref()) {
        *value = round2(raw, gauss_shift);
      }
      let luma_coeff = coeffs.get(luma_count).copied().unwrap_or(0) as i32;
      let luma = &luma;
      auto_regress(
        &mut plane,
        chroma_w,
        chroma_h,
        lag,
        &coeffs[..luma_count.min(coeffs.len())],
        ar_shift,
        (grain_min, grain_max),
        |x, y| {
          if !has_luma {
            return 0;
          }
          let luma_x = ((x - 3) << sx) + 3;
          let luma_y = ((y - 3) << sy) + 3;
          let mut total = 0;
          for j in 0..=sy {
            for i in 0..=sx {
              total += luma[(luma_y + j) * SIZE + luma_x + i];
            }
          }
          luma_coeff * round2(total, (sx + sy) as u32)
        },
      );
      stats.chroma_std[index] += deviation(&plane, chroma_w, chroma_h).0 / ROUNDS as f64;
    }
  }
  stats.luma_std = stats.luma_std.sqrt();
  stats.chroma_std = stats.chroma_std.map(f64::sqrt);
  stats
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn white_template_has_the_gaussian_deviation() {
    let params = GrainParams {
      scaling_points_y: vec![[0, 20]],
      ..GrainParams::default()
    };
    let stats = simulate(&params, 10, true, true, [true, true]);
    assert!((stats.luma_std - 128.0).abs() < 3.0, "{}", stats.luma_std);
    assert!((stats.chroma_std[0] - 128.0).abs() < 4.0);
    assert!(stats.luma_rho[0].abs() < 0.03);
    let stats = simulate(&params, 8, true, true, [false, false]);
    assert!((stats.luma_std - 32.0).abs() < 1.0, "{}", stats.luma_std);
  }

  #[test]
  fn correlated_template_is_stronger_and_smoother() {
    let params = GrainParams {
      ar_coeff_lag: 1,
      ar_coeff_shift: 7,
      ar_coeffs_y: vec![0, 32, 0, 48],
      scaling_points_y: vec![[0, 20]],
      ..GrainParams::default()
    };
    let stats = simulate(&params, 10, true, true, [false, false]);
    assert!(stats.luma_std > 140.0);
    assert!(stats.luma_rho[0] > 0.3);
  }
}
