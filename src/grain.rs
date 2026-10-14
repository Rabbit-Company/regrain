use crate::av1::bits::{BitReader, BitWriter};
use anyhow::{Result, bail};

pub const MAX_LUMA_POINTS: usize = 14;
pub const MAX_CHROMA_POINTS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GrainParams {
  pub ar_coeff_lag: u8,
  pub ar_coeff_shift: u8,
  pub grain_scale_shift: u8,
  pub scaling_shift: u8,
  pub chroma_scaling_from_luma: bool,
  pub overlap_flag: bool,
  pub clip_to_restricted_range: bool,
  pub cb_mult: u8,
  pub cb_luma_mult: u8,
  pub cb_offset: u16,
  pub cr_mult: u8,
  pub cr_luma_mult: u8,
  pub cr_offset: u16,
  pub scaling_points_y: Vec<[u8; 2]>,
  pub scaling_points_cb: Vec<[u8; 2]>,
  pub scaling_points_cr: Vec<[u8; 2]>,
  pub ar_coeffs_y: Vec<i8>,
  pub ar_coeffs_cb: Vec<i8>,
  pub ar_coeffs_cr: Vec<i8>,
}

impl Default for GrainParams {
  fn default() -> Self {
    Self {
      ar_coeff_lag: 0,
      ar_coeff_shift: 6,
      grain_scale_shift: 0,
      scaling_shift: 8,
      chroma_scaling_from_luma: false,
      overlap_flag: true,
      clip_to_restricted_range: false,
      cb_mult: 128,
      cb_luma_mult: 192,
      cb_offset: 256,
      cr_mult: 128,
      cr_luma_mult: 192,
      cr_offset: 256,
      scaling_points_y: Vec::new(),
      scaling_points_cb: Vec::new(),
      scaling_points_cr: Vec::new(),
      ar_coeffs_y: Vec::new(),
      ar_coeffs_cb: vec![0],
      ar_coeffs_cr: vec![0],
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChromaFormat {
  pub mono_chrome: bool,
  pub subsampling_x: bool,
  pub subsampling_y: bool,
}

impl GrainParams {
  pub fn luma_coeff_count(&self) -> usize {
    let lag = self.ar_coeff_lag as usize;
    2 * lag * (lag + 1)
  }

  pub fn is_empty(&self) -> bool {
    self.scaling_points_y.is_empty()
      && self.scaling_points_cb.is_empty()
      && self.scaling_points_cr.is_empty()
      && !self.chroma_scaling_from_luma
  }

  pub fn validate(&self) -> Result<()> {
    if self.ar_coeff_lag > 3 {
      bail!("ar_coeff_lag must be between 0 and 3");
    }
    if !(6..=9).contains(&self.ar_coeff_shift) {
      bail!("ar_coeff_shift must be between 6 and 9");
    }
    if self.grain_scale_shift > 3 {
      bail!("grain_scale_shift must be between 0 and 3");
    }
    if !(8..=11).contains(&self.scaling_shift) {
      bail!("scaling_shift must be between 8 and 11");
    }
    if self.cb_offset > 511 || self.cr_offset > 511 {
      bail!("chroma offsets must be between 0 and 511");
    }
    for (name, points, max) in [
      ("luma", &self.scaling_points_y, MAX_LUMA_POINTS),
      ("cb", &self.scaling_points_cb, MAX_CHROMA_POINTS),
      ("cr", &self.scaling_points_cr, MAX_CHROMA_POINTS),
    ] {
      if points.len() > max {
        bail!("too many {name} scaling points ({}, limit {max})", points.len());
      }
      if points.windows(2).any(|pair| pair[1][0] <= pair[0][0]) {
        bail!("{name} scaling points must have increasing positions");
      }
    }
    let luma = self.luma_coeff_count();
    if self.ar_coeffs_y.len() != luma && !(self.scaling_points_y.is_empty() && self.ar_coeffs_y.is_empty()) {
      bail!("expected {luma} luma coefficients, found {}", self.ar_coeffs_y.len());
    }
    for (name, coeffs) in [("cb", &self.ar_coeffs_cb), ("cr", &self.ar_coeffs_cr)] {
      if !coeffs.is_empty() && coeffs.len() != luma + 1 {
        bail!("expected {} {name} coefficients, found {}", luma + 1, coeffs.len());
      }
    }
    Ok(())
  }

  // Every rule the AV1 specification places on film grain parameters for this format.
  pub fn conformance_errors(&self, format: ChromaFormat) -> Vec<String> {
    let mut errors = Vec::new();
    let mut check = |ok: bool, message: &str| {
      if !ok {
        errors.push(message.to_string());
      }
    };
    let (y, cb, cr) = (&self.scaling_points_y, &self.scaling_points_cb, &self.scaling_points_cr);
    check(y.len() <= MAX_LUMA_POINTS, "more than 14 luma scaling points");
    check(
      cb.len() <= MAX_CHROMA_POINTS && cr.len() <= MAX_CHROMA_POINTS,
      "more than 10 chroma scaling points",
    );
    for points in [y, cb, cr] {
      check(
        points.windows(2).all(|pair| pair[1][0] > pair[0][0]),
        "scaling point positions do not increase",
      );
    }
    check(
      !format.mono_chrome || !self.chroma_scaling_from_luma,
      "monochrome video cannot scale chroma from luma",
    );
    check(
      !format.mono_chrome || (cb.is_empty() && cr.is_empty()),
      "monochrome video has chroma scaling points",
    );
    check(
      !self.chroma_scaling_from_luma || (cb.is_empty() && cr.is_empty()),
      "chroma scaled from luma also has its own points",
    );
    let subsampled = format.subsampling_x && format.subsampling_y;
    check(
      !subsampled || !y.is_empty() || (cb.is_empty() && cr.is_empty()),
      "4:2:0 chroma points without luma points",
    );
    check(
      !subsampled || cb.is_empty() == cr.is_empty(),
      "4:2:0 video has points for only one chroma plane",
    );
    check(self.ar_coeff_lag <= 3, "ar_coeff_lag above 3");
    check((6..=9).contains(&self.ar_coeff_shift), "ar_coeff_shift outside 6 to 9");
    check(self.grain_scale_shift <= 3, "grain_scale_shift above 3");
    check((8..=11).contains(&self.scaling_shift), "scaling_shift outside 8 to 11");
    check(self.cb_offset <= 511 && self.cr_offset <= 511, "chroma offset above 511");
    let luma = self.luma_coeff_count();
    let chroma = if y.is_empty() { luma } else { luma + 1 };
    check(
      self.ar_coeffs_y.len() == if y.is_empty() { 0 } else { luma },
      "wrong number of luma coefficients",
    );
    let cb_active = self.chroma_scaling_from_luma || !cb.is_empty();
    let cr_active = self.chroma_scaling_from_luma || !cr.is_empty();
    check(
      self.ar_coeffs_cb.len() == if cb_active { chroma } else { 0 },
      "wrong number of cb coefficients",
    );
    check(
      self.ar_coeffs_cr.len() == if cr_active { chroma } else { 0 },
      "wrong number of cr coefficients",
    );
    errors
  }

  // Drops everything the bitstream cannot carry for this chroma format, so
  // that parameters compare equal after a write and read cycle.
  pub fn normalized(&self, format: ChromaFormat) -> GrainParams {
    let mut p = self.clone();
    if format.mono_chrome {
      p.chroma_scaling_from_luma = false;
    }
    let no_chroma_points =
      format.mono_chrome || p.chroma_scaling_from_luma || (format.subsampling_x && format.subsampling_y && p.scaling_points_y.is_empty());
    if no_chroma_points {
      p.scaling_points_cb.clear();
      p.scaling_points_cr.clear();
    } else if format.subsampling_x && format.subsampling_y && p.scaling_points_cb.is_empty() != p.scaling_points_cr.is_empty() {
      // In 4:2:0 both chroma planes must have scaling points or neither may. A
      // single point at zero strength keeps the other plane free of grain.
      let empty = if p.scaling_points_cb.is_empty() {
        &mut p.scaling_points_cb
      } else {
        &mut p.scaling_points_cr
      };
      empty.push([128, 0]);
    }
    let luma = p.luma_coeff_count();
    if p.scaling_points_y.is_empty() {
      p.ar_coeffs_y.clear();
    } else {
      p.ar_coeffs_y.resize(luma, 0);
    }
    let chroma = if p.scaling_points_y.is_empty() { luma } else { luma + 1 };
    let fix = |coeffs: &mut Vec<i8>, active: bool| {
      if active {
        let luma_coeff = if coeffs.len() == luma + 1 { coeffs[luma] } else { 0 };
        coeffs.resize(luma, 0);
        if chroma > luma {
          coeffs.push(luma_coeff);
        }
      } else {
        coeffs.clear();
      }
    };
    let cb_active = p.chroma_scaling_from_luma || !p.scaling_points_cb.is_empty();
    let cr_active = p.chroma_scaling_from_luma || !p.scaling_points_cr.is_empty();
    fix(&mut p.ar_coeffs_cb, cb_active);
    fix(&mut p.ar_coeffs_cr, cr_active);
    if p.scaling_points_cb.is_empty() {
      p.cb_mult = 128;
      p.cb_luma_mult = 192;
      p.cb_offset = 256;
    }
    if p.scaling_points_cr.is_empty() {
      p.cr_mult = 128;
      p.cr_luma_mult = 192;
      p.cr_offset = 256;
    }
    p
  }

  // Reads the part of film_grain_params() that follows update_grain.
  pub fn read(r: &mut BitReader, format: ChromaFormat) -> Result<GrainParams> {
    let mut p = GrainParams {
      ar_coeffs_cb: Vec::new(),
      ar_coeffs_cr: Vec::new(),
      ..GrainParams::default()
    };
    let read_points = |r: &mut BitReader, max: usize| -> Result<Vec<[u8; 2]>> {
      let count = r.bits(4)? as usize;
      if count > max {
        bail!("invalid film grain scaling point count {count}");
      }
      let mut points = Vec::with_capacity(count);
      for _ in 0..count {
        points.push([r.bits(8)? as u8, r.bits(8)? as u8]);
      }
      Ok(points)
    };
    p.scaling_points_y = read_points(r, MAX_LUMA_POINTS)?;
    p.chroma_scaling_from_luma = if format.mono_chrome { false } else { r.bit()? };
    let no_chroma_points =
      format.mono_chrome || p.chroma_scaling_from_luma || (format.subsampling_x && format.subsampling_y && p.scaling_points_y.is_empty());
    if !no_chroma_points {
      p.scaling_points_cb = read_points(r, MAX_CHROMA_POINTS)?;
      p.scaling_points_cr = read_points(r, MAX_CHROMA_POINTS)?;
    }
    p.scaling_shift = r.bits(2)? as u8 + 8;
    p.ar_coeff_lag = r.bits(2)? as u8;
    let luma = p.luma_coeff_count();
    let read_coeffs =
      |r: &mut BitReader, count: usize| -> Result<Vec<i8>> { (0..count).map(|_| Ok((r.bits(8)? as i32 - 128) as i8)).collect() };
    let mut chroma = luma;
    if !p.scaling_points_y.is_empty() {
      chroma = luma + 1;
      p.ar_coeffs_y = read_coeffs(r, luma)?;
    }
    if p.chroma_scaling_from_luma || !p.scaling_points_cb.is_empty() {
      p.ar_coeffs_cb = read_coeffs(r, chroma)?;
    }
    if p.chroma_scaling_from_luma || !p.scaling_points_cr.is_empty() {
      p.ar_coeffs_cr = read_coeffs(r, chroma)?;
    }
    p.ar_coeff_shift = r.bits(2)? as u8 + 6;
    p.grain_scale_shift = r.bits(2)? as u8;
    if !p.scaling_points_cb.is_empty() {
      p.cb_mult = r.bits(8)? as u8;
      p.cb_luma_mult = r.bits(8)? as u8;
      p.cb_offset = r.bits(9)? as u16;
    }
    if !p.scaling_points_cr.is_empty() {
      p.cr_mult = r.bits(8)? as u8;
      p.cr_luma_mult = r.bits(8)? as u8;
      p.cr_offset = r.bits(9)? as u16;
    }
    p.overlap_flag = r.bit()?;
    p.clip_to_restricted_range = r.bit()?;
    Ok(p)
  }

  // Writes the part of film_grain_params() that follows update_grain.
  // The parameters must already be normalized for the stream's format.
  pub fn write(&self, w: &mut BitWriter, format: ChromaFormat) {
    let write_points = |w: &mut BitWriter, points: &[[u8; 2]]| {
      w.bits(points.len() as u32, 4);
      for point in points {
        w.bits(point[0] as u32, 8);
        w.bits(point[1] as u32, 8);
      }
    };
    write_points(w, &self.scaling_points_y);
    if !format.mono_chrome {
      w.bit(self.chroma_scaling_from_luma);
    }
    let no_chroma_points = format.mono_chrome
      || self.chroma_scaling_from_luma
      || (format.subsampling_x && format.subsampling_y && self.scaling_points_y.is_empty());
    if !no_chroma_points {
      write_points(w, &self.scaling_points_cb);
      write_points(w, &self.scaling_points_cr);
    }
    w.bits(self.scaling_shift as u32 - 8, 2);
    w.bits(self.ar_coeff_lag as u32, 2);
    for coeffs in [&self.ar_coeffs_y, &self.ar_coeffs_cb, &self.ar_coeffs_cr] {
      for &coeff in coeffs {
        w.bits((coeff as i32 + 128) as u32, 8);
      }
    }
    w.bits(self.ar_coeff_shift as u32 - 6, 2);
    w.bits(self.grain_scale_shift as u32, 2);
    if !self.scaling_points_cb.is_empty() {
      w.bits(self.cb_mult as u32, 8);
      w.bits(self.cb_luma_mult as u32, 8);
      w.bits(self.cb_offset as u32, 9);
    }
    if !self.scaling_points_cr.is_empty() {
      w.bits(self.cr_mult as u32, 8);
      w.bits(self.cr_luma_mult as u32, 8);
      w.bits(self.cr_offset as u32, 9);
    }
    w.bit(self.overlap_flag);
    w.bit(self.clip_to_restricted_range);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> GrainParams {
    GrainParams {
      ar_coeff_lag: 3,
      ar_coeff_shift: 8,
      scaling_shift: 11,
      scaling_points_y: vec![[0, 20], [128, 40], [255, 30]],
      scaling_points_cb: vec![[0, 10], [255, 12]],
      scaling_points_cr: vec![[16, 9]],
      ar_coeffs_y: (0..24).map(|i| i as i8 - 12).collect(),
      ar_coeffs_cb: (0..25).map(|i| -(i as i8)).collect(),
      ar_coeffs_cr: (0..25).map(|i| i as i8 * 2).collect(),
      ..GrainParams::default()
    }
  }

  fn round_trip(params: &GrainParams, format: ChromaFormat) -> GrainParams {
    let normalized = params.normalized(format);
    let mut w = BitWriter::new();
    normalized.write(&mut w, format);
    w.trailing_bits();
    let data = w.finish();
    let read = GrainParams::read(&mut BitReader::new(&data), format).unwrap();
    assert_eq!(read.normalized(format), normalized);
    read
  }

  #[test]
  fn round_trips_420() {
    let format = ChromaFormat {
      mono_chrome: false,
      subsampling_x: true,
      subsampling_y: true,
    };
    let read = round_trip(&sample(), format);
    assert_eq!(read.scaling_points_cr, vec![[16, 9]]);
    assert_eq!(read.ar_coeffs_cb.len(), 25);
  }

  #[test]
  fn drops_chroma_for_monochrome_and_lumaless_420() {
    let mono = ChromaFormat {
      mono_chrome: true,
      subsampling_x: true,
      subsampling_y: true,
    };
    let read = round_trip(&sample(), mono);
    assert!(read.scaling_points_cb.is_empty() && read.ar_coeffs_cb.is_empty());

    let format = ChromaFormat {
      mono_chrome: false,
      subsampling_x: true,
      subsampling_y: true,
    };
    let mut params = sample();
    params.scaling_points_y.clear();
    let read = round_trip(&params, format);
    assert!(read.scaling_points_cb.is_empty() && read.ar_coeffs_y.is_empty());
  }

  #[test]
  fn gives_both_420_chroma_planes_points() {
    let format = ChromaFormat {
      mono_chrome: false,
      subsampling_x: true,
      subsampling_y: true,
    };
    let mut params = sample();
    params.scaling_points_cr.clear();
    params.ar_coeffs_cr.clear();
    let read = round_trip(&params, format);
    assert_eq!(read.scaling_points_cr, vec![[128, 0]]);
    assert_eq!(read.ar_coeffs_cr, vec![0; 25]);
    assert_eq!(read.scaling_points_cb, sample().scaling_points_cb);

    let format_422 = ChromaFormat {
      mono_chrome: false,
      subsampling_x: true,
      subsampling_y: false,
    };
    assert!(round_trip(&params, format_422).scaling_points_cr.is_empty());
  }

  struct Rng(u64);

  impl Rng {
    fn below(&mut self, limit: u64) -> u64 {
      self.0 ^= self.0 << 13;
      self.0 ^= self.0 >> 7;
      self.0 ^= self.0 << 17;
      self.0 % limit
    }

    fn points(&mut self, max: u64) -> Vec<[u8; 2]> {
      let count = self.below(max + 1);
      let mut x = 0u64;
      let mut points = Vec::new();
      for _ in 0..count {
        x += 1 + self.below(40);
        if x >= 256 {
          break;
        }
        points.push([x as u8, self.below(256) as u8]);
      }
      points
    }

    fn coeffs(&mut self, count: usize) -> Vec<i8> {
      (0..count).map(|_| self.below(256) as u8 as i8).collect()
    }
  }

  #[test]
  fn normalized_parameters_always_conform() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let formats = [
      ChromaFormat {
        mono_chrome: true,
        subsampling_x: true,
        subsampling_y: true,
      },
      ChromaFormat {
        mono_chrome: false,
        subsampling_x: true,
        subsampling_y: true,
      },
      ChromaFormat {
        mono_chrome: false,
        subsampling_x: true,
        subsampling_y: false,
      },
      ChromaFormat {
        mono_chrome: false,
        subsampling_x: false,
        subsampling_y: false,
      },
    ];
    for _ in 0..20_000 {
      let lag = rng.below(4) as u8;
      let luma = 2 * lag as usize * (lag as usize + 1);
      let cb_count = luma + rng.below(2) as usize;
      let params = GrainParams {
        ar_coeff_lag: lag,
        ar_coeff_shift: 6 + rng.below(4) as u8,
        grain_scale_shift: rng.below(4) as u8,
        scaling_shift: 8 + rng.below(4) as u8,
        chroma_scaling_from_luma: rng.below(4) == 0,
        cb_offset: rng.below(512) as u16,
        cr_offset: rng.below(512) as u16,
        scaling_points_y: rng.points(14),
        scaling_points_cb: rng.points(10),
        scaling_points_cr: rng.points(10),
        ar_coeffs_y: rng.coeffs(luma),
        ar_coeffs_cb: rng.coeffs(cb_count),
        ar_coeffs_cr: rng.coeffs(luma + 1),
        ..GrainParams::default()
      };
      let format = formats[rng.below(4) as usize];
      let normalized = params.normalized(format);
      assert_eq!(normalized.conformance_errors(format), Vec::<String>::new(), "{params:?} {format:?}");
      assert_eq!(round_trip(&params, format), normalized.clone());
      assert_eq!(normalized.normalized(format), normalized);
    }
  }

  #[test]
  fn reports_broken_rules() {
    let format = ChromaFormat {
      mono_chrome: false,
      subsampling_x: true,
      subsampling_y: true,
    };
    let mut params = sample();
    params.scaling_points_cr.clear();
    params.scaling_points_y.push([10, 1]);
    let errors = params.conformance_errors(format);
    assert!(errors.iter().any(|error| error.contains("only one chroma plane")), "{errors:?}");
    assert!(errors.iter().any(|error| error.contains("do not increase")), "{errors:?}");
  }

  #[test]
  fn keeps_chroma_without_luma_in_444() {
    let format = ChromaFormat {
      mono_chrome: false,
      subsampling_x: false,
      subsampling_y: false,
    };
    let mut params = sample();
    params.scaling_points_y.clear();
    let read = round_trip(&params, format);
    assert_eq!(read.scaling_points_cb.len(), 2);
    assert_eq!(read.ar_coeffs_cb.len(), 24);
  }
}
