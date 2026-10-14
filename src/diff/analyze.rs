use super::probe::PixelFormat;

pub const BLOCK: usize = 32;
pub const BINS: usize = 32;
pub const BUCKETS_PER_OCTAVE: f64 = 8.0;
pub const LOG_MIN: f64 = -14.0;
pub const BUCKETS: usize = 210;
pub const MAX_DX: i32 = 6;
pub const MAX_DY: i32 = 3;
pub const LAGS: usize = 7 + 3 * 13;
pub const CROSS: usize = 25;
const FIXED_SHIFT: i32 = 3;
const FIXED_LIMIT: i32 = 4095;
const FLAT_FLOOR: f64 = 2.0;
const CORRELATION_BLOCKS: usize = 320;
const FLAT_BASELINE_RATIO: f64 = 3.0;

pub fn lag_index(dx: i32, dy: i32) -> usize {
  let (dx, dy) = if dy < 0 || (dy == 0 && dx < 0) { (-dx, -dy) } else { (dx, dy) };
  if dy == 0 {
    dx as usize
  } else {
    (7 + (dy - 1) * 13 + dx + MAX_DX) as usize
  }
}

// Causal neighbours in the order AV1 stores autoregression coefficients.
pub fn ar_offsets(lag: i32) -> Vec<(i32, i32)> {
  let mut offsets = Vec::new();
  for dy in -lag..=0 {
    for dx in -lag..=lag {
      if dy == 0 && dx == 0 {
        break;
      }
      offsets.push((dx, dy));
    }
  }
  offsets
}

pub fn variance_bucket(variance: f64) -> usize {
  if variance <= 0.0 {
    return 0;
  }
  let bucket = ((variance.log2() - LOG_MIN) * BUCKETS_PER_OCTAVE).floor() + 1.0;
  bucket.clamp(1.0, (BUCKETS - 1) as f64) as usize
}

pub fn bucket_variance(position: f64) -> f64 {
  if position < 1.0 {
    return 0.0;
  }
  ((position - 1.0) / BUCKETS_PER_OCTAVE + LOG_MIN).exp2()
}

#[derive(Clone, Copy, Debug)]
pub struct Layout {
  pub width: usize,
  pub height: usize,
  pub format: PixelFormat,
}

impl Layout {
  pub fn planes(&self) -> usize {
    if self.format.mono { 1 } else { 3 }
  }

  pub fn plane_size(&self, plane: usize) -> (usize, usize) {
    if plane == 0 {
      return (self.width, self.height);
    }
    let sx = usize::from(self.format.subsampling_x);
    let sy = usize::from(self.format.subsampling_y);
    ((self.width + sx) >> sx, (self.height + sy) >> sy)
  }

  pub fn frame_samples(&self) -> usize {
    (0..self.planes()).map(|plane| self.plane_size(plane)).map(|(w, h)| w * h).sum()
  }

  pub fn grid(&self) -> (usize, usize) {
    (self.width / BLOCK, self.height / BLOCK)
  }
}

#[derive(Clone, Debug)]
pub struct PlaneStats {
  pub histogram: Vec<u32>,
  // Blocks without any grain in the source, such as letterbox bars, by luma level.
  pub constant: Vec<u32>,
  pub correlation: [f64; LAGS],
  pub weight: f64,
  pub cross: [f64; CROSS],
  pub luma_power: f64,
  pub cross_weight: f64,
  pub blocks: u64,
}

impl Default for PlaneStats {
  fn default() -> Self {
    Self {
      histogram: vec![0; BINS * BUCKETS],
      constant: vec![0; 256],
      correlation: [0.0; LAGS],
      weight: 0.0,
      cross: [0.0; CROSS],
      luma_power: 0.0,
      cross_weight: 0.0,
      blocks: 0,
    }
  }
}

impl PlaneStats {
  pub fn add(&mut self, other: &PlaneFrame) {
    for &slot in &other.observations {
      self.histogram[slot as usize] += 1;
    }
    for &level in &other.constant {
      self.constant[level as usize] += 1;
    }
    self.blocks += other.observations.len() as u64;
    for i in 0..LAGS {
      self.correlation[i] += other.correlation[i];
    }
    for i in 0..CROSS {
      self.cross[i] += other.cross[i];
    }
    self.weight += other.weight;
    self.luma_power += other.luma_power;
    self.cross_weight += other.cross_weight;
  }

  pub fn merge(&mut self, other: &PlaneStats) {
    for (a, b) in self.histogram.iter_mut().zip(&other.histogram) {
      *a += b;
    }
    for (a, b) in self.constant.iter_mut().zip(&other.constant) {
      *a += b;
    }
    for i in 0..LAGS {
      self.correlation[i] += other.correlation[i];
    }
    for i in 0..CROSS {
      self.cross[i] += other.cross[i];
    }
    self.weight += other.weight;
    self.luma_power += other.luma_power;
    self.cross_weight += other.cross_weight;
    self.blocks += other.blocks;
  }
}

#[derive(Clone, Debug)]
pub struct PlaneFrame {
  pub observations: Vec<u16>,
  pub constant: Vec<u8>,
  pub correlation: [f64; LAGS],
  pub weight: f64,
  pub cross: [f64; CROSS],
  pub luma_power: f64,
  pub cross_weight: f64,
}

impl Default for PlaneFrame {
  fn default() -> Self {
    Self {
      observations: Vec::new(),
      constant: Vec::new(),
      correlation: [0.0; LAGS],
      weight: 0.0,
      cross: [0.0; CROSS],
      luma_power: 0.0,
      cross_weight: 0.0,
    }
  }
}

// `planes` holds the strictly flat blocks of every plane, followed by the loosely flat ones.
#[derive(Clone, Debug, Default)]
pub struct FrameResult {
  pub index: u64,
  pub mismatch: f64,
  pub thumbnail: Vec<u8>,
  pub source_thumbnail: Vec<u8>,
  pub planes: Vec<PlaneFrame>,
}

pub trait Sample: Copy + Send + Sync {
  fn value(self) -> i32;
}

impl Sample for u8 {
  fn value(self) -> i32 {
    self as i32
  }
}

impl Sample for u16 {
  fn value(self) -> i32 {
    self as i32
  }
}

#[derive(Default)]
pub struct Scratch {
  luma_residual: Vec<i16>,
  luma_bins: Vec<u8>,
  luma_flat: Vec<u8>,
  luma_constant: Vec<bool>,
  loose: Vec<PlaneFrame>,
  block: Vec<i16>,
  partner: Vec<i16>,
  infos: Vec<BlockInfo>,
  textures: Vec<f64>,
}

struct BlockInfo {
  mean: f64,
  texture: f64,
  constant_source: bool,
  residual_sum: i64,
  residual_variance: f64,
}

fn block_info<T: Sample>(source: &[T], denoised: &[T], stride: usize, x0: usize, y0: usize, bw: usize, bh: usize) -> BlockInfo {
  let mut sum = 0i64;
  let mut sum_sq = 0i64;
  let mut sum_x = 0i64;
  let mut sum_y = 0i64;
  let mut residual_sum = 0i64;
  let mut residual_sq = 0i64;
  let mut source_min = i32::MAX;
  let mut source_max = i32::MIN;
  for y in 0..bh {
    let start = (y0 + y) * stride + x0;
    let den_row = &denoised[start..start + bw];
    let src_row = &source[start..start + bw];
    // Sample values are clamped to 12 bits, so a row of up to 32 of them fits these 32 bit sums.
    let mut row_sum = 0i32;
    let mut row_sq = 0i32;
    let mut row_x = 0i32;
    let mut row_res = 0i32;
    let mut row_res_sq = 0i32;
    let mut row_min = i32::MAX;
    let mut row_max = i32::MIN;
    for (x, (&d, &s)) in den_row.iter().zip(src_row).enumerate() {
      let d = d.value() & 0xfff;
      let s = s.value() & 0xfff;
      let r = s - d;
      row_sum += d;
      row_sq += d * d;
      row_x += d * x as i32;
      row_res += r;
      row_res_sq += r * r;
      row_min = row_min.min(s);
      row_max = row_max.max(s);
    }
    sum += row_sum as i64;
    sum_sq += row_sq as i64;
    sum_x += row_x as i64;
    sum_y += row_sum as i64 * y as i64;
    residual_sum += row_res as i64;
    residual_sq += row_res_sq as i64;
    source_min = source_min.min(row_min);
    source_max = source_max.max(row_max);
  }
  let n = (bw * bh) as f64;
  let mean = sum as f64 / n;
  let variance = (sum_sq as f64 / n - mean * mean).max(0.0);
  let mean_x = (bw as f64 - 1.0) / 2.0;
  let mean_y = (bh as f64 - 1.0) / 2.0;
  let var_x = (bw as f64 * bw as f64 - 1.0) / 12.0;
  let var_y = (bh as f64 * bh as f64 - 1.0) / 12.0;
  let cov_x = sum_x as f64 / n - mean * mean_x;
  let cov_y = sum_y as f64 / n - mean * mean_y;
  let texture = (variance - cov_x * cov_x / var_x - cov_y * cov_y / var_y).max(0.0);
  let residual_mean = residual_sum as f64 / n;
  let residual_variance = (residual_sq as f64 / n - residual_mean * residual_mean).max(0.0);
  BlockInfo {
    mean,
    texture,
    constant_source: source_min == source_max,
    residual_sum,
    residual_variance,
  }
}

#[inline]
fn dot(a: &[i16], b: &[i16]) -> i64 {
  let mut total = 0i32;
  for (&x, &y) in a.iter().zip(b) {
    total += x as i32 * y as i32;
  }
  total as i64
}

fn centre(values: &mut [i16], shift_done: bool, sum: i64) -> i64 {
  let n = values.len() as i64;
  let scale = if shift_done { 1 } else { 1 << FIXED_SHIFT };
  let total = sum * scale;
  let mean = if total >= 0 { (total + n / 2) / n } else { (total - n / 2) / n } as i32;
  let mut power = 0i64;
  for value in values.iter_mut() {
    let centred = (*value as i32 * scale as i32 - mean).clamp(-FIXED_LIMIT, FIXED_LIMIT);
    *value = centred as i16;
    power += (centred * centred) as i64;
  }
  power
}

fn correlate(block: &[i16], bw: usize, bh: usize, power: i64, weight: f64, out: &mut [f64; LAGS]) {
  let variance = power as f64 / (bw * bh) as f64;
  for dy in 0..=MAX_DY as usize {
    let first = if dy == 0 { 0 } else { -MAX_DX };
    for dx in first..=MAX_DX {
      let shift = dx.unsigned_abs() as usize;
      let mut total = 0i64;
      for y in 0..bh - dy {
        let a = &block[y * bw..(y + 1) * bw];
        let b = &block[(y + dy) * bw..(y + dy + 1) * bw];
        total += if dx >= 0 {
          dot(&a[..bw - shift], &b[shift..])
        } else {
          dot(&a[shift..], &b[..bw - shift])
        };
      }
      let pairs = ((bw - shift) * (bh - dy)) as f64;
      out[lag_index(dx, dy as i32)] += weight * total as f64 / pairs / variance;
    }
  }
}

fn cross_correlate(chroma: &[i16], luma: &[i16], bw: usize, bh: usize, scale: f64, weight: f64, out: &mut [f64; CROSS]) {
  let mut offsets = ar_offsets(MAX_DY);
  offsets.push((0, 0));
  for (i, &(dx, dy)) in offsets.iter().enumerate() {
    let down = (-dy) as usize;
    let shift = dx.unsigned_abs() as usize;
    let mut total = 0i64;
    for y in down..bh {
      let l = &luma[y * bw..(y + 1) * bw];
      let c = &chroma[(y - down) * bw..(y - down + 1) * bw];
      total += if dx >= 0 {
        dot(&l[..bw - shift], &c[shift..])
      } else {
        dot(&l[shift..], &c[..bw - shift])
      };
    }
    let pairs = ((bw - shift) * (bh - down)) as f64;
    out[i] += weight * total as f64 / pairs / scale;
  }
}

pub fn analyze<T: Sample>(layout: &Layout, index: u64, source: &[T], denoised: &[T], scratch: &mut Scratch) -> FrameResult {
  let (grid_w, grid_h) = layout.grid();
  let depth_scale = (1u32 << (layout.format.bit_depth - 8)) as f64;
  let variance_scale = depth_scale * depth_scale;
  let noise_floor = 1.0 / 6.0;
  let mut result = FrameResult {
    index,
    mismatch: 0.0,
    source_thumbnail: vec![0; grid_w * grid_h],
    thumbnail: vec![0; grid_w * grid_h],
    planes: Vec::new(),
  };
  let sx = usize::from(layout.format.subsampling_x);
  let sy = usize::from(layout.format.subsampling_y);
  let (width, height) = (layout.width, layout.height);

  scratch.luma_residual.resize(width * height, 0);
  for ((residual, &s), &d) in scratch
    .luma_residual
    .iter_mut()
    .zip(&source[..width * height])
    .zip(&denoised[..width * height])
  {
    *residual = (s.value() - d.value()).clamp(-FIXED_LIMIT, FIXED_LIMIT) as i16;
  }
  scratch.luma_bins.resize(grid_w * grid_h, 0);
  scratch.luma_flat.resize(grid_w * grid_h, 0);
  scratch.luma_constant.resize(grid_w * grid_h, false);
  scratch.block.resize(BLOCK * BLOCK, 0);
  scratch.partner.resize(BLOCK * BLOCK, 0);

  let mut offset = 0;
  for plane in 0..layout.planes() {
    let (pw, ph) = layout.plane_size(plane);
    let src = &source[offset..offset + pw * ph];
    let den = &denoised[offset..offset + pw * ph];
    offset += pw * ph;
    let (bw, bh) = if plane == 0 { (BLOCK, BLOCK) } else { (BLOCK >> sx, BLOCK >> sy) };
    let mut strict = PlaneFrame::default();
    let mut loose = PlaneFrame::default();
    scratch.infos.clear();
    scratch.textures.clear();
    for by in 0..grid_h {
      for bx in 0..grid_w {
        let info = block_info(src, den, pw, bx * bw, by * bh, bw, bh);
        if plane == 0 {
          result.mismatch += (info.residual_sum as f64 / (bw * bh) as f64 / depth_scale).abs() / (grid_w * grid_h) as f64;
          let level = (info.mean / depth_scale).clamp(0.0, 255.0);
          result.thumbnail[by * grid_w + bx] = level as u8;
          let source_mean = info.mean + info.residual_sum as f64 / (bw * bh) as f64;
          result.source_thumbnail[by * grid_w + bx] = (source_mean / depth_scale).clamp(0.0, 255.0) as u8;
          scratch.luma_bins[by * grid_w + bx] = (level as usize * BINS / 256) as u8;
        }
        if !info.constant_source {
          scratch.textures.push(info.texture);
        }
        scratch.infos.push(info);
      }
    }
    // A block is strictly flat when the denoised picture in it hardly varies. Blocks
    // within a few times the flattest tenth of the frame are loosely flat. A scene uses
    // the loose ones only when it has too few strict ones, since they can hold detail.
    let strict_limit = FLAT_FLOOR * variance_scale;
    let mut loose_limit = strict_limit;
    if !scratch.textures.is_empty() {
      let rank = scratch.textures.len() / 10;
      let (_, baseline, _) = scratch.textures.select_nth_unstable_by(rank, f64::total_cmp);
      loose_limit = loose_limit.max(FLAT_BASELINE_RATIO * *baseline);
    }
    let candidates = scratch
      .infos
      .iter()
      .filter(|info| !info.constant_source && info.texture <= loose_limit && info.residual_variance > 0.0)
      .count();
    let stride = candidates.div_ceil(CORRELATION_BLOCKS).max(1);
    let mut correlated = 0usize;
    for by in 0..grid_h {
      for bx in 0..grid_w {
        let cell = by * grid_w + bx;
        let info = &scratch.infos[cell];
        if plane == 0 {
          scratch.luma_constant[cell] = info.constant_source;
        }
        // AV1 scales chroma grain by luma as well, so both planes have to be free of grain.
        if info.constant_source && scratch.luma_constant[cell] {
          strict.constant.push(result.source_thumbnail[cell]);
        }
        let mut flatness = match info.texture {
          _ if info.constant_source => 0,
          texture if texture <= strict_limit => 2,
          texture if texture <= loose_limit => 1,
          _ => 0,
        };
        if plane == 0 {
          scratch.luma_flat[cell] = flatness;
        }
        // Denoisers change colour along luma edges, so chroma is measured where luma is flat too.
        flatness = flatness.min(scratch.luma_flat[cell]);
        if flatness == 0 {
          continue;
        }
        let stats = if flatness == 2 { &mut strict } else { &mut loose };
        let bin = scratch.luma_bins[cell] as usize;
        let bucket = variance_bucket(info.residual_variance / variance_scale);
        stats.observations.push((bin * BUCKETS + bucket) as u16);
        if info.residual_variance <= 0.0 {
          continue;
        }
        // The histogram takes every flat block. The correlation needs far fewer to be exact.
        correlated += 1;
        if !(correlated - 1).is_multiple_of(stride) {
          continue;
        }
        let block = &mut scratch.block[..bw * bh];
        for y in 0..bh {
          let start = (by * bh + y) * pw + bx * bw;
          for x in 0..bw {
            block[y * bw + x] = (src[start + x].value() - den[start + x].value()).clamp(-FIXED_LIMIT, FIXED_LIMIT) as i16;
          }
        }
        let power = centre(block, false, info.residual_sum);
        if power == 0 {
          continue;
        }
        let weight = info.residual_variance / (info.residual_variance + noise_floor);
        correlate(block, bw, bh, power, weight, &mut stats.correlation);
        stats.weight += weight;
        if plane == 0 {
          continue;
        }
        let luma = &mut scratch.partner[..bw * bh];
        let mut luma_sum = 0i64;
        let mut luma_full_sum = 0i64;
        let mut luma_full_sq = 0i64;
        for y in 0..bh {
          for x in 0..bw {
            let mut total = 0i32;
            for j in 0..=sy {
              let row = (((by * bh + y) << sy) + j) * width + ((bx * bw + x) << sx);
              for i in 0..=sx {
                let value = scratch.luma_residual[row + i] as i32;
                total += value;
                luma_full_sq += (value * value) as i64;
              }
            }
            luma_full_sum += total as i64;
            let scaled = (total << FIXED_SHIFT) >> (sx + sy);
            luma[y * bw + x] = scaled.clamp(-FIXED_LIMIT * 8, FIXED_LIMIT * 8) as i16;
            luma_sum += scaled as i64;
          }
        }
        let full_count = ((bw << sx) * (bh << sy)) as f64;
        let full_mean = luma_full_sum as f64 / full_count;
        let luma_variance = luma_full_sq as f64 / full_count - full_mean * full_mean;
        if luma_variance <= 0.0 {
          continue;
        }
        let luma_power = centre(luma, true, luma_sum);
        let fixed = (1 << FIXED_SHIFT) as f64;
        let chroma_std = (power as f64 / (bw * bh) as f64).sqrt();
        let luma_std = luma_variance.sqrt() * fixed;
        cross_correlate(block, luma, bw, bh, chroma_std * luma_std, weight, &mut stats.cross);
        stats.luma_power += weight * luma_power as f64 / (bw * bh) as f64 / (luma_std * luma_std);
        stats.cross_weight += weight;
      }
    }
    result.planes.push(strict);
    scratch.loose.push(loose);
  }
  result.planes.append(&mut scratch.loose);
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lag_indices_are_unique_and_symmetric() {
    let mut seen = [false; LAGS];
    for dy in 0..=MAX_DY {
      for dx in -MAX_DX..=MAX_DX {
        if dy == 0 && dx < 0 {
          continue;
        }
        let index = lag_index(dx, dy);
        assert!(!seen[index]);
        seen[index] = true;
        assert_eq!(lag_index(-dx, -dy), index);
      }
    }
    assert!(seen.iter().all(|&s| s));
  }

  #[test]
  fn offsets_follow_the_coefficient_order() {
    assert_eq!(ar_offsets(0).len(), 0);
    assert_eq!(ar_offsets(1), vec![(-1, -1), (0, -1), (1, -1), (-1, 0)]);
    assert_eq!(ar_offsets(3).len(), 24);
  }

  #[test]
  fn variance_buckets_round_trip() {
    assert_eq!(variance_bucket(0.0), 0);
    for variance in [0.001, 0.07, 1.0, 9.5, 300.0] {
      let bucket = variance_bucket(variance);
      let low = bucket_variance(bucket as f64);
      let high = bucket_variance(bucket as f64 + 1.0);
      assert!(
        low <= variance * 1.0001 && variance <= high * 1.0001,
        "{variance} not in {low}..{high}"
      );
    }
  }

  fn noise_frame(layout: &Layout, sigma: f64, seed: u64) -> (Vec<u16>, Vec<u16>) {
    let mut state = seed;
    let mut next = || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut gaussian = || {
      let (u, v) = (next().max(1e-12), next());
      (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    };
    let samples = layout.frame_samples();
    let luma = layout.width * layout.height;
    let denoised: Vec<u16> = (0..samples)
      .map(|i| {
        if i < luma {
          300 + ((i % layout.width) / 64) as u16 * 40
        } else {
          512
        }
      })
      .collect();
    let source = denoised
      .iter()
      .map(|&d| (d as f64 + sigma * gaussian()).round().clamp(0.0, 1023.0) as u16)
      .collect();
    (source, denoised)
  }

  #[test]
  fn measures_white_noise() {
    let format = PixelFormat {
      bit_depth: 10,
      mono: false,
      subsampling_x: true,
      subsampling_y: true,
    };
    let layout = Layout {
      width: 256,
      height: 128,
      format,
    };
    let (source, denoised) = noise_frame(&layout, 4.0, 99);
    let result = analyze(&layout, 0, &source, &denoised, &mut Scratch::default());
    let luma = &result.planes[0];
    assert_eq!(luma.observations.len(), 32);
    let mean_log: f64 = luma
      .observations
      .iter()
      .map(|&slot| bucket_variance((slot as usize % BUCKETS) as f64 + 0.5).ln())
      .sum::<f64>()
      / 32.0;
    assert!((mean_log.exp().sqrt() - 1.0).abs() < 0.08, "sigma {}", mean_log.exp().sqrt());
    let rho = luma.correlation[lag_index(1, 0)] / luma.weight;
    assert!(rho.abs() < 0.05, "rho {rho}");
    assert!((luma.correlation[0] / luma.weight - 1.0).abs() < 1e-6);
    assert_eq!(result.planes.len(), 6);
    assert!(
      result.planes[3..].iter().all(|plane| plane.observations.is_empty()),
      "flat blocks must count as strictly flat"
    );
    assert!(result.planes[1].weight > 0.0);
  }
}
