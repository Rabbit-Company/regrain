use crate::grain::GrainParams;
use crate::report::Coded;
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::path::Path;

pub const TICKS_PER_SECOND: u64 = 10_000_000;
pub const OPEN_END: u64 = i64::MAX as u64;

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
  pub start: u64,
  pub end: u64,
  pub seed: u16,
  pub params: GrainParams,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrainTable {
  pub segments: Vec<Segment>,
}

impl GrainTable {
  pub fn read(path: &Path) -> Result<GrainTable> {
    let text = std::fs::read_to_string(path)
      .with_context(|| format!("failed to read {}", path.display()))
      .code("table_not_found")?;
    Self::parse(&text)
      .with_context(|| format!("invalid film grain table {}", path.display()))
      .code("invalid_table")
  }

  // Written under a temporary name first, so a failed write never leaves half a table.
  pub fn write(&self, path: &Path) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    let temp = path.with_file_name(name);
    let result = std::fs::write(&temp, self.to_string())
      .and_then(|()| std::fs::rename(&temp, path))
      .with_context(|| format!("failed to write {}", path.display()))
      .code("io_error");
    if result.is_err() {
      let _ = std::fs::remove_file(&temp);
    }
    result
  }

  pub fn parse(text: &str) -> Result<GrainTable> {
    let mut lines = text.lines().map(str::trim_end).filter(|line| !line.trim().is_empty());
    if lines.next().map(str::trim) != Some("filmgrn1") {
      bail!("missing filmgrn1 header");
    }
    let mut segments: Vec<Segment> = Vec::new();
    // Per entry: whether grain is applied, whether it reuses the previous entry's
    // parameters, and whether it has parameter lines of its own.
    let mut flags: Vec<(bool, bool, bool)> = Vec::new();
    for line in lines {
      let mut fields = line.split_whitespace();
      let Some(tag) = fields.next() else { continue };
      let values: Vec<&str> = fields.collect();
      if tag == "E" {
        if values.len() < 5 {
          bail!("segment header needs 5 fields: {line}");
        }
        let start: u64 = values[0].parse().context("invalid segment start")?;
        let end: u64 = values[1].parse().context("invalid segment end")?;
        if end <= start {
          bail!("segment ends before it starts: {line}");
        }
        let apply = values[2].parse::<u8>().context("invalid apply_grain")? != 0;
        let seed = values[3].parse::<u32>().context("invalid random seed")? as u16;
        let update = values[4].parse::<u8>().context("invalid update_parameters")? != 0;
        flags.push((apply, !update, false));
        segments.push(Segment {
          start,
          end,
          seed,
          params: GrainParams {
            ar_coeffs_cb: Vec::new(),
            ar_coeffs_cr: Vec::new(),
            ..GrainParams::default()
          },
        });
        continue;
      }
      let Some(segment) = segments.last_mut() else {
        bail!("parameters found before the first segment: {line}");
      };
      if let Some(last) = flags.last_mut() {
        last.2 = true;
      }
      let p = &mut segment.params;
      let numbers = || -> Result<Vec<i64>> {
        values
          .iter()
          .map(|v| v.parse::<i64>().with_context(|| format!("invalid number in: {line}")))
          .collect()
      };
      let points = || -> Result<Vec<[u8; 2]>> {
        let n = numbers()?;
        let count = *n.first().context("missing point count")? as usize;
        if n.len() != 1 + 2 * count {
          bail!("point count does not match the values: {line}");
        }
        n[1..].chunks(2).map(|c| Ok([u8::try_from(c[0])?, u8::try_from(c[1])?])).collect()
      };
      let coeffs = || -> Result<Vec<i8>> { numbers()?.into_iter().map(|v| Ok(i8::try_from(v)?)).collect() };
      match tag {
        "p" => {
          let n = numbers()?;
          if n.len() < 12 {
            bail!("parameter line needs 12 fields: {line}");
          }
          p.ar_coeff_lag = u8::try_from(n[0])?;
          p.ar_coeff_shift = u8::try_from(n[1])?;
          p.grain_scale_shift = u8::try_from(n[2])?;
          p.scaling_shift = u8::try_from(n[3])?;
          p.chroma_scaling_from_luma = n[4] != 0;
          p.overlap_flag = n[5] != 0;
          p.cb_mult = u8::try_from(n[6])?;
          p.cb_luma_mult = u8::try_from(n[7])?;
          p.cb_offset = u16::try_from(n[8])?;
          p.cr_mult = u8::try_from(n[9])?;
          p.cr_luma_mult = u8::try_from(n[10])?;
          p.cr_offset = u16::try_from(n[11])?;
        }
        "sY" => p.scaling_points_y = points()?,
        "sCb" => p.scaling_points_cb = points()?,
        "sCr" => p.scaling_points_cr = points()?,
        "cY" => p.ar_coeffs_y = coeffs()?,
        "cCb" => p.ar_coeffs_cb = coeffs()?,
        "cCr" => p.ar_coeffs_cr = coeffs()?,
        _ => bail!("unknown line: {line}"),
      }
    }
    let mut previous: Option<GrainParams> = None;
    let mut kept = Vec::with_capacity(segments.len());
    for (mut segment, (apply, inherit, has_params)) in segments.into_iter().zip(flags) {
      if inherit && !has_params {
        let Some(params) = &previous else {
          bail!(
            "the entry starting at {} reuses earlier parameters, but there are none",
            segment.start
          );
        };
        segment.params = params.clone();
      }
      previous = Some(segment.params.clone());
      if apply {
        kept.push(segment);
      }
    }
    let segments = kept;
    for segment in &segments {
      segment
        .params
        .validate()
        .with_context(|| format!("segment starting at {}", segment.start))?;
    }
    if segments.windows(2).any(|pair| pair[1].start < pair[0].end) {
      bail!("segments overlap or are out of order");
    }
    Ok(GrainTable { segments })
  }

  pub fn find(&self, timestamp: u64) -> Option<usize> {
    let index = self.segments.partition_point(|segment| segment.end <= timestamp);
    (index < self.segments.len() && self.segments[index].start <= timestamp).then_some(index)
  }
}

impl std::fmt::Display for GrainTable {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut out = String::from("filmgrn1\n");
    for segment in &self.segments {
      let p = &segment.params;
      let luma = p.luma_coeff_count();
      let _ = writeln!(out, "E {} {} 1 {} 1", segment.start, segment.end, segment.seed);
      let _ = writeln!(
        out,
        "\tp {} {} {} {} {} {} {} {} {} {} {} {}",
        p.ar_coeff_lag,
        p.ar_coeff_shift,
        p.grain_scale_shift,
        p.scaling_shift,
        u8::from(p.chroma_scaling_from_luma),
        u8::from(p.overlap_flag),
        p.cb_mult,
        p.cb_luma_mult,
        p.cb_offset,
        p.cr_mult,
        p.cr_luma_mult,
        p.cr_offset
      );
      for (tag, points) in [
        ("sY", &p.scaling_points_y),
        ("sCb", &p.scaling_points_cb),
        ("sCr", &p.scaling_points_cr),
      ] {
        let _ = write!(out, "\t{tag} {}{}", points.len(), if tag == "sY" { " " } else { "" });
        for point in points {
          let _ = write!(out, " {} {}", point[0], point[1]);
        }
        out.push('\n');
      }
      for (tag, coeffs, count) in [
        ("cY", &p.ar_coeffs_y, luma),
        ("cCb", &p.ar_coeffs_cb, luma + 1),
        ("cCr", &p.ar_coeffs_cr, luma + 1),
      ] {
        let _ = write!(out, "\t{tag}");
        for i in 0..count {
          let _ = write!(out, " {}", coeffs.get(i).copied().unwrap_or(0));
        }
        out.push('\n');
      }
    }
    f.write_str(&out)
  }
}

pub fn frame_start(frame: u64, fps_num: u64, fps_den: u64) -> u64 {
  (frame as u128 * fps_den as u128 * TICKS_PER_SECOND as u128 / fps_num as u128) as u64
}

#[cfg(test)]
mod tests {
  use super::*;

  const SAMPLE: &str = "filmgrn1\nE 0 417083 1 10956 1\n\tp 3 9 0 11 0 1 128 192 256 128 192 256\n\tsY 2  0 27 255 27\n\tsCb 2 0 22 255 20\n\tsCr 2 0 23 255 22\n\tcY 2 1 2 3 3 1 2 1 3 3 1 2 2 1 1 4 5 -2 5 1 3 3 -1 -1\n\tcCb 2 4 0 4 1 2 4 -1 4 2 10 2 7 1 3 1 6 33 13 2 1 5 8 26 2\n\tcCr 3 0 0 7 3 0 3 -1 2 4 6 3 6 2 1 -1 8 30 12 2 1 4 8 26 1\nE 417083 9223372036854775807 1 0 1\n\tp 0 6 0 8 0 1 128 192 256 128 192 256\n\tsY 0 \n\tsCb 0\n\tsCr 0\n\tcY\n\tcCb 0\n\tcCr 0\n";

  #[test]
  fn parses_and_prints_the_same_table() {
    let table = GrainTable::parse(SAMPLE).unwrap();
    assert_eq!(table.segments.len(), 2);
    assert_eq!(table.segments[0].params.ar_coeffs_cb.len(), 25);
    assert_eq!(table.segments[0].seed, 10956);
    assert_eq!(table.to_string(), SAMPLE);
    assert_eq!(GrainTable::parse(&table.to_string()).unwrap(), table);
  }

  #[test]
  fn reuses_parameters_like_libaom() {
    let text = "filmgrn1\nE 0 100 1 7 1\n\tp 0 6 0 8 0 1 128 192 256 128 192 256\n\tsY 1  0 20\n\tsCb 0\n\tsCr 0\n\tcY\n\tcCb 0\n\tcCr 0\nE 100 200 1 9 0\nE 200 300 0 9 1\nE 300 400 1 11 0\n";
    let table = GrainTable::parse(text).unwrap();
    assert_eq!(table.segments.len(), 3);
    assert_eq!(table.segments[1].params, table.segments[0].params);
    assert_eq!(table.segments[1].seed, 9);
    assert_eq!(table.segments[2].start, 300);
    assert!(table.segments[2].params.is_empty());
    assert!(GrainTable::parse("filmgrn1\nE 0 100 1 7 0\n").is_err());
  }

  #[test]
  fn finds_segments_by_time() {
    let table = GrainTable::parse(SAMPLE).unwrap();
    assert_eq!(table.find(0), Some(0));
    assert_eq!(table.find(417082), Some(0));
    assert_eq!(table.find(417083), Some(1));
    assert_eq!(table.find(u64::MAX), None);
  }

  #[test]
  fn rejects_bad_tables() {
    assert!(GrainTable::parse("E 0 1 1 0 1\n").is_err());
    assert!(GrainTable::parse("filmgrn1\nE 5 5 1 0 1\n").is_err());
    assert!(GrainTable::parse("filmgrn1\nE 0 9 1 0 1\n\tp 4 6 0 8 0 1 128 192 256 128 192 256\n").is_err());
  }

  #[test]
  fn frame_boundaries_use_whole_ticks() {
    assert_eq!(frame_start(1, 24000, 1001), 417083);
    assert_eq!(frame_start(20, 24000, 1001), 8341666);
  }
}
