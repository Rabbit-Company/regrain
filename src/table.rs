use crate::report::Coded;
use anyhow::{Context, Result, anyhow, bail};
use rabbit_av1::grain::GrainParams;
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

// One E line of a table as written, before the parameters are checked.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
  pub line: usize,
  pub apply: bool,
  // update_parameters was 0, so the parameters were taken from the entry before.
  pub reused: bool,
  pub segment: Segment,
}

// A table as written: its entries, and where the filmgrn1 line was.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
  // Line of the filmgrn1 header. Anything above it is a preamble of comments,
  // which the table convention allows but libaom, SVT-AV1 and grav1synth do not.
  pub header_line: usize,
  pub entries: Vec<Entry>,
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
    let entries = parse_document(text)?.entries;
    // An entry that ends where it starts covers no frame, as in libaom, which
    // looks entries up by start <= time < end.
    let segments: Vec<Segment> = entries
      .into_iter()
      .filter(|entry| entry.apply && entry.segment.end > entry.segment.start)
      .map(|entry| entry.segment)
      .collect();
    for segment in &segments {
      segment
        .params
        .validate()
        // A table error, which gets the code of the table it is in.
        .map_err(|error| anyhow!(error.to_string()))
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

// Reads every entry of a table with the line it starts on, including entries
// without grain, and fills in parameters an entry reuses from the one before.
// Only the syntax is checked here.
//
// Lines before the filmgrn1 header are a preamble and are skipped, as the
// table convention allows. A header of another version, such as filmgrn2,
// is refused rather than read with the wrong grammar.
pub fn parse_document(text: &str) -> Result<Document> {
  let text = text.strip_prefix('\u{feff}').unwrap_or(text);
  let mut lines = text
    .lines()
    .enumerate()
    .map(|(index, line)| (index + 1, line.trim_end()))
    .filter(|(_, line)| !line.trim().is_empty());
  let header_line = loop {
    let Some((number, line)) = lines.next() else {
      bail!("missing filmgrn1 header");
    };
    match line.trim() {
      "filmgrn1" => break number,
      other if is_version_header(other) => {
        bail!("line {number}: {other} is a table version regrain does not read, it reads filmgrn1")
      }
      _ => {}
    }
  };
  let mut entries: Vec<Entry> = Vec::new();
  // Per entry: whether it has parameter lines of its own.
  let mut has_params: Vec<bool> = Vec::new();
  for (number, line) in lines {
    if is_version_header(line.trim()) {
      bail!("line {number}: a second {} header, a table has one", line.trim());
    }
    parse_line(line, number, &mut entries, &mut has_params).with_context(|| format!("line {number}"))?;
  }
  let mut previous: Option<GrainParams> = None;
  for (entry, has_params) in entries.iter_mut().zip(has_params) {
    if entry.reused && !has_params {
      let Some(params) = &previous else {
        bail!("line {}: the entry reuses earlier parameters, but there are none", entry.line);
      };
      entry.segment.params = params.clone();
    } else {
      entry.reused = false;
    }
    previous = Some(entry.segment.params.clone());
  }
  Ok(Document { header_line, entries })
}

// filmgrn followed by a version number.
fn is_version_header(line: &str) -> bool {
  line
    .strip_prefix("filmgrn")
    .is_some_and(|version| !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()))
}

fn parse_line(line: &str, number: usize, entries: &mut Vec<Entry>, has_params: &mut Vec<bool>) -> Result<()> {
  let mut fields = line.split_whitespace();
  let Some(tag) = fields.next() else { return Ok(()) };
  let values: Vec<&str> = fields.collect();
  if tag == "E" {
    if values.len() < 5 {
      bail!("segment header needs 5 fields: {line}");
    }
    let start: u64 = values[0].parse().context("invalid segment start")?;
    let end: u64 = values[1].parse().context("invalid segment end")?;
    if end < start {
      bail!("segment ends before it starts: {line}");
    }
    let apply = values[2].parse::<u8>().context("invalid apply_grain")? != 0;
    let seed = values[3].parse::<u32>().context("invalid random seed")? as u16;
    let update = values[4].parse::<u8>().context("invalid update_parameters")? != 0;
    has_params.push(false);
    entries.push(Entry {
      line: number,
      apply,
      reused: !update,
      segment: Segment {
        start,
        end,
        seed,
        params: GrainParams {
          ar_coeffs_cb: Vec::new(),
          ar_coeffs_cr: Vec::new(),
          ..GrainParams::default()
        },
      },
    });
    return Ok(());
  }
  let Some(entry) = entries.last_mut() else {
    bail!("parameters found before the first segment: {line}");
  };
  if let Some(last) = has_params.last_mut() {
    *last = true;
  }
  let p = &mut entry.segment.params;
  let numbers = || -> Result<Vec<i64>> {
    values
      .iter()
      .map(|v| v.parse::<i64>().with_context(|| format!("invalid number in: {line}")))
      .collect()
  };
  let points = || -> Result<Vec<[u8; 2]>> {
    let n = numbers()?;
    let count = usize::try_from(*n.first().context("missing point count")?).ok();
    if count
      .and_then(|count| count.checked_mul(2))
      .and_then(|values| values.checked_add(1))
      != Some(n.len())
    {
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
  Ok(())
}

// Written in the layout of the FGS table convention: parameter lines indented by
// two spaces and single spaces between values. libaom, SVT-AV1 and grav1synth
// read it as they read libaom's own layout with tabs.
impl std::fmt::Display for GrainTable {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut out = String::from("filmgrn1\n");
    for segment in &self.segments {
      let p = &segment.params;
      let luma = p.luma_coeff_count();
      let _ = writeln!(out, "E {} {} 1 {} 1", segment.start, segment.end, segment.seed);
      let _ = writeln!(
        out,
        "  p {} {} {} {} {} {} {} {} {} {} {} {}",
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
        let _ = write!(out, "  {tag} {}", points.len());
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
        let _ = write!(out, "  {tag}");
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

  const SAMPLE: &str = "filmgrn1\nE 0 417083 1 10956 1\n  p 3 9 0 11 0 1 128 192 256 128 192 256\n  sY 2 0 27 255 27\n  sCb 2 0 22 255 20\n  sCr 2 0 23 255 22\n  cY 2 1 2 3 3 1 2 1 3 3 1 2 2 1 1 4 5 -2 5 1 3 3 -1 -1\n  cCb 2 4 0 4 1 2 4 -1 4 2 10 2 7 1 3 1 6 33 13 2 1 5 8 26 2\n  cCr 3 0 0 7 3 0 3 -1 2 4 6 3 6 2 1 -1 8 30 12 2 1 4 8 26 1\nE 417083 9223372036854775807 1 0 1\n  p 0 6 0 8 0 1 128 192 256 128 192 256\n  sY 0\n  sCb 0\n  sCr 0\n  cY\n  cCb 0\n  cCr 0\n";

  // The same table as libaom writes it, with tabs and two spaces after the sY count.
  const LIBAOM: &str = "filmgrn1\nE 0 417083 1 10956 1\n\tp 3 9 0 11 0 1 128 192 256 128 192 256\n\tsY 2  0 27 255 27\n\tsCb 2 0 22 255 20\n\tsCr 2 0 23 255 22\n\tcY 2 1 2 3 3 1 2 1 3 3 1 2 2 1 1 4 5 -2 5 1 3 3 -1 -1\n\tcCb 2 4 0 4 1 2 4 -1 4 2 10 2 7 1 3 1 6 33 13 2 1 5 8 26 2\n\tcCr 3 0 0 7 3 0 3 -1 2 4 6 3 6 2 1 -1 8 30 12 2 1 4 8 26 1\nE 417083 9223372036854775807 1 0 1\n\tp 0 6 0 8 0 1 128 192 256 128 192 256\n\tsY 0 \n\tsCb 0\n\tsCr 0\n\tcY\n\tcCb 0\n\tcCr 0\n";

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
  fn reads_libaom_layout_and_writes_the_convention() {
    let table = GrainTable::parse(LIBAOM).unwrap();
    assert_eq!(table, GrainTable::parse(SAMPLE).unwrap());
    assert_eq!(table.to_string(), SAMPLE);
    // Carriage returns, a byte order mark and blank lines are tolerated.
    let windows = format!("\u{feff}{}", SAMPLE.replace('\n', "\r\n\r\n"));
    assert_eq!(GrainTable::parse(&windows).unwrap(), table);
  }

  #[test]
  fn skips_a_preamble_and_refuses_other_versions() {
    let commented = format!("# made by FGSEditor\n# grain for episode 1\n\n{SAMPLE}");
    let document = parse_document(&commented).unwrap();
    assert_eq!(document.header_line, 4);
    assert_eq!(document.entries[0].line, 5);
    assert_eq!(GrainTable::parse(&commented).unwrap(), GrainTable::parse(SAMPLE).unwrap());
    assert_eq!(parse_document(SAMPLE).unwrap().header_line, 1);

    let error = GrainTable::parse(&SAMPLE.replace("filmgrn1", "filmgrn2")).unwrap_err();
    assert!(
      format!("{error:#}").contains("filmgrn2 is a table version regrain does not read"),
      "{error:#}"
    );
    let twice = format!("{SAMPLE}filmgrn1\n");
    let error = GrainTable::parse(&twice).unwrap_err();
    assert!(format!("{error:#}").contains("a second filmgrn1 header"), "{error:#}");
    assert!(GrainTable::parse("# only a comment\n").is_err());
    // A filmgrn line that is not a version is an ordinary preamble line.
    assert!(GrainTable::parse(&format!("filmgrn tables\n{SAMPLE}")).is_ok());
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
    let reused: Vec<bool> = parse_document(text).unwrap().entries.iter().map(|entry| entry.reused).collect();
    assert_eq!(reused, [false, true, false, true]);
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
    assert!(GrainTable::parse("filmgrn1\nE 5 4 1 0 1\n").is_err());
    assert!(GrainTable::parse("filmgrn1\nE 5 5 1 0 1\n").unwrap().segments.is_empty());
    let error = GrainTable::parse("filmgrn1\nE 0 9 1 0 1\n\n\tsY 1 x\n").unwrap_err();
    assert!(format!("{error:#}").starts_with("line 4: invalid number"), "{error:#}");
    assert!(GrainTable::parse("filmgrn1\nE 0 9 1 0 1\n\tp 4 6 0 8 0 1 128 192 256 128 192 256\n").is_err());
  }

  #[test]
  fn frame_boundaries_use_whole_ticks() {
    assert_eq!(frame_start(1, 24000, 1001), 417083);
    assert_eq!(frame_start(20, 24000, 1001), 8341666);
  }
}
