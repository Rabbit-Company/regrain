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

fn round2(value: i32, shift: u32) -> i32 {
  if shift == 0 { value } else { (value + (1 << (shift - 1))) >> shift }
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

  for round in 0..ROUNDS {
    let mut gaussian = Gaussian(0x9e37_79b9_7f4a_7c15 ^ (round + 1).wrapping_mul(0xd1b5_4a32_d192_ed03));
    if has_luma {
      for value in luma.iter_mut() {
        *value = round2(gaussian.next(), gauss_shift);
      }
      for y in 3..SIZE {
        for x in 3..SIZE - 3 {
          let mut sum = 0;
          let mut pos = 0;
          'taps: for dy in -lag..=0 {
            for dx in -lag..=lag {
              if dy == 0 && dx == 0 {
                break 'taps;
              }
              let neighbour = luma[(y as i32 + dy) as usize * SIZE + (x as i32 + dx) as usize];
              sum += params.ar_coeffs_y.get(pos).copied().unwrap_or(0) as i32 * neighbour;
              pos += 1;
            }
          }
          let index = y * SIZE + x;
          luma[index] = (luma[index] + round2(sum, ar_shift)).clamp(grain_min, grain_max);
        }
      }
      let (variance, horizontal, vertical) = deviation(&luma, SIZE, SIZE);
      stats.luma_std += variance / ROUNDS as f64;
      stats.luma_rho[0] += horizontal / ROUNDS as f64;
      stats.luma_rho[1] += vertical / ROUNDS as f64;
    }
    for (index, coeffs) in [&params.ar_coeffs_cb, &params.ar_coeffs_cr].into_iter().enumerate() {
      if !chroma[index] {
        continue;
      }
      for value in plane.iter_mut() {
        *value = round2(gaussian.next(), gauss_shift);
      }
      for y in 3..chroma_h {
        for x in 3..chroma_w - 3 {
          let mut sum = 0;
          let mut pos = 0;
          'taps: for dy in -lag..=0 {
            for dx in -lag..=lag {
              if dy == 0 && dx == 0 {
                break 'taps;
              }
              let neighbour = plane[(y as i32 + dy) as usize * chroma_w + (x as i32 + dx) as usize];
              sum += coeffs.get(pos).copied().unwrap_or(0) as i32 * neighbour;
              pos += 1;
            }
          }
          if has_luma {
            let luma_x = ((x - 3) << sx) + 3;
            let luma_y = ((y - 3) << sy) + 3;
            let mut total = 0;
            for j in 0..=sy {
              for i in 0..=sx {
                total += luma[(luma_y + j) * SIZE + luma_x + i];
              }
            }
            sum += coeffs.get(luma_count).copied().unwrap_or(0) as i32 * round2(total, (sx + sy) as u32);
          }
          let index = y * chroma_w + x;
          plane[index] = (plane[index] + round2(sum, ar_shift)).clamp(grain_min, grain_max);
        }
      }
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
