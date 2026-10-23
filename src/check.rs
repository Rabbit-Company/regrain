use crate::stats::clock;
use crate::table::{Document, GrainTable, Segment, TICKS_PER_SECOND};
use rabbit_av1::grain::{ChromaFormat, GrainParams, MAX_CHROMA_POINTS, MAX_LUMA_POINTS};

// Tables carry no chroma format, so they are checked for 4:2:0, the format of
// nearly every AV1 encode.
const FORMAT: ChromaFormat = ChromaFormat {
  mono_chrome: false,
  subsampling_x: true,
  subsampling_y: true,
};

// libaom's default seed, given to entries whose seed of zero would stop the grain.
const REPLACEMENT_SEED: u16 = 7391;

// How much stronger the grain may be at the bottom of a template than at the top
// before the filter counts as unstable. Stable filters stay below 1.04 on real
// tables, a filter whose coefficients sum to the divisor reaches 1.2 to 1.4.
const UNSTABLE_SPREAD: f64 = 1.2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
  // apply refuses the table.
  Error,
  // apply accepts the table, but the grain is not what the table seems to say.
  Warning,
}

impl Severity {
  pub fn name(self) -> &'static str {
    match self {
      Severity::Error => "error",
      Severity::Warning => "warning",
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
  pub severity: Severity,
  pub code: &'static str,
  // Counted from 1 over every E line of the table, including entries without
  // grain. None for findings about the table as a whole.
  pub entry: Option<usize>,
  pub line: usize,
  pub start: Option<u64>,
  pub message: String,
  pub fixable: bool,
}

impl Finding {
  pub fn start_seconds(&self) -> Option<f64> {
    self.start.map(|start| start as f64 / TICKS_PER_SECOND as f64)
  }
}

pub struct Report {
  pub findings: Vec<Finding>,
  pub entries: usize,
  // The corrected table, when every error can be corrected.
  pub fixed: Option<GrainTable>,
}

impl Report {
  pub fn count(&self, severity: Severity) -> usize {
    self.findings.iter().filter(|finding| finding.severity == severity).count()
  }
}

// Checks a table, with the filters of its entries simulated on `threads`
// threads. `progress` is called with the number of entries simulated, as
// crate::parallel::map does.
pub fn check(document: &Document, threads: usize, progress: impl FnMut(usize)) -> anyhow::Result<Report> {
  // The only slow part, and the same for an entry wherever it sits in the table.
  let spreads = crate::parallel::map(
    &document.entries,
    threads,
    |entry| {
      let p = &entry.segment.params;
      // Filters AV1 does not allow are reported as errors and not simulated.
      (entry.apply && p.ar_coeff_lag <= 3 && (6..=9).contains(&p.ar_coeff_shift))
        .then(|| unstable_spread(p))
        .flatten()
    },
    progress,
  )?;
  let mut findings = Vec::new();
  if document.header_line > 1 {
    findings.push(Finding {
      severity: Severity::Warning,
      code: "preamble",
      entry: None,
      line: 1,
      start: None,
      message: format!(
        "lines 1 to {} come before filmgrn1; libaom then encodes without grain and SVT-AV1 and grav1synth refuse the table",
        document.header_line - 1
      ),
      fixable: true,
    });
  }
  let mut fixed = Vec::new();
  let mut previous_end: Option<u64> = None;
  let mut marked_gap = false;
  for (index, entry) in document.entries.iter().enumerate() {
    if !entry.apply {
      marked_gap = true;
      continue;
    }
    let segment = &entry.segment;
    let mut add = |severity: Severity, code: &'static str, fixable: bool, message: String| {
      findings.push(Finding {
        severity,
        code,
        entry: Some(index + 1),
        line: entry.line,
        start: Some(segment.start),
        message,
        fixable,
      });
    };
    if segment.end == segment.start {
      add(
        Severity::Warning,
        "empty_entry",
        true,
        "the entry ends where it starts, so it covers no frame and is ignored".to_string(),
      );
      continue;
    }
    if entry.reused {
      add(
        Severity::Warning,
        "reused_parameters",
        true,
        "update_parameters is 0, so the entry takes the parameters of the entry before it, but libaom gives it no grain; --fix writes them out".to_string(),
      );
    }
    match previous_end {
      Some(end) if segment.start < end => add(
        Severity::Error,
        "overlap",
        false,
        format!(
          "starts at {}, before the entry before it ends at {}",
          clock(segment.start as f64 / TICKS_PER_SECOND as f64),
          clock(end as f64 / TICKS_PER_SECOND as f64)
        ),
      ),
      Some(end) if segment.start > end && !marked_gap => add(
        Severity::Warning,
        "gap",
        false,
        format!(
          "no entry covers {} to {}, so those frames get no grain",
          clock(end as f64 / TICKS_PER_SECOND as f64),
          clock(segment.start as f64 / TICKS_PER_SECOND as f64)
        ),
      ),
      _ => {}
    }
    previous_end = Some(segment.end);
    marked_gap = false;
    fixed.push(check_params(segment, spreads[index], &mut add));
  }
  let unfixable = findings
    .iter()
    .any(|finding| finding.severity == Severity::Error && !finding.fixable);
  Ok(Report {
    entries: fixed.len(),
    fixed: (!unfixable).then_some(GrainTable { segments: fixed }),
    findings,
  })
}

// Reports what is wrong with one entry's parameters and returns the entry as
// --fix writes it.
// `spread` is what unstable_spread returns for the entry's parameters.
fn check_params(segment: &Segment, spread: Option<f64>, add: &mut impl FnMut(Severity, &'static str, bool, String)) -> Segment {
  let p = &segment.params;
  let mut out = segment.clone();
  let mut valid = true;
  let mut range = |name: &str, value: u16, low: u16, high: u16| {
    if !(low..=high).contains(&value) {
      add(
        Severity::Error,
        "out_of_range",
        false,
        format!("{name} is {value}, AV1 allows {low} to {high}"),
      );
      valid = false;
    }
  };
  range("ar_coeff_lag", p.ar_coeff_lag as u16, 0, 3);
  range("ar_coeff_shift", p.ar_coeff_shift as u16, 6, 9);
  range("grain_scale_shift", p.grain_scale_shift as u16, 0, 3);
  range("scaling_shift", p.scaling_shift as u16, 8, 11);
  range("cb_offset", p.cb_offset, 0, 511);
  range("cr_offset", p.cr_offset, 0, 511);

  let planes = [
    ("luma", &p.scaling_points_y, MAX_LUMA_POINTS),
    ("Cb", &p.scaling_points_cb, MAX_CHROMA_POINTS),
    ("Cr", &p.scaling_points_cr, MAX_CHROMA_POINTS),
  ];
  for (plane, (name, points, max)) in planes.into_iter().enumerate() {
    if points.len() > max {
      add(
        Severity::Error,
        "too_many_points",
        false,
        format!("{} {name} scaling points, AV1 allows {max}", points.len()),
      );
      valid = false;
    }
    if points.windows(2).any(|pair| pair[1][0] <= pair[0][0]) {
      let mut sorted = points.clone();
      sorted.sort_by_key(|point| point[0]);
      let distinct = sorted.windows(2).all(|pair| pair[1][0] > pair[0][0]);
      let message = if distinct {
        format!("{name} scaling points are not in order of brightness")
      } else {
        format!("{name} scaling points have two values for the same brightness")
      };
      add(Severity::Error, "points_out_of_order", distinct, message);
      valid = false;
      let target = [
        &mut out.params.scaling_points_y,
        &mut out.params.scaling_points_cb,
        &mut out.params.scaling_points_cr,
      ];
      *target.into_iter().nth(plane).unwrap() = sorted;
    }
  }

  if p.ar_coeff_lag <= 3 {
    let luma = p.luma_coeff_count();
    if p.ar_coeffs_y.len() != luma && !(p.scaling_points_y.is_empty() && p.ar_coeffs_y.is_empty()) {
      add(
        Severity::Error,
        "coefficient_count",
        false,
        format!("{} luma coefficients, lag {} needs {luma}", p.ar_coeffs_y.len(), p.ar_coeff_lag),
      );
      valid = false;
    }
    for (name, coeffs) in [("Cb", &p.ar_coeffs_cb), ("Cr", &p.ar_coeffs_cr)] {
      if !coeffs.is_empty() && coeffs.len() != luma + 1 {
        add(
          Severity::Error,
          "coefficient_count",
          false,
          format!("{} {name} coefficients, lag {} needs {}", coeffs.len(), p.ar_coeff_lag, luma + 1),
        );
        valid = false;
      }
    }
  }

  let has_chroma_points = !p.scaling_points_cb.is_empty() || !p.scaling_points_cr.is_empty();
  if p.chroma_scaling_from_luma && has_chroma_points {
    add(
      Severity::Warning,
      "chroma_points_ignored",
      true,
      "chroma is scaled from luma, so the Cb and Cr scaling points are ignored".to_string(),
    );
  } else if p.scaling_points_y.is_empty() && has_chroma_points {
    add(
      Severity::Warning,
      "chroma_without_luma",
      true,
      "the entry has no luma points, and 4:2:0 AV1 drops chroma grain without luma grain".to_string(),
    );
  }
  if segment.seed == 0 && !p.scaling_points_y.is_empty() {
    add(
      Severity::Warning,
      "zero_seed",
      true,
      format!(
        "seed 0 makes a decoder repeat one value, so luma gets a constant offset instead of grain on frames with this seed; --fix uses {REPLACEMENT_SEED}"
      ),
    );
    out.seed = REPLACEMENT_SEED;
  }

  if valid && let Some(spread) = spread {
    add(
      Severity::Warning,
      "unstable_filter",
      false,
      format!(
        "the autoregressive filter is unstable: grain at the bottom of the decoder's template is {spread:.1} times as strong as at the top, so grain blocks differ in strength and look patchy"
      ),
    );
  }

  out.params = out.params.normalized(FORMAT);
  out
}

// Grows a filter's grain over a template of the decoder's size without the
// decoder's limits, and returns how much stronger it is in the bottom rows a
// decoder reads than in the top ones, for the plane where that is highest,
// when it is above UNSTABLE_SPREAD.
fn unstable_spread(params: &GrainParams) -> Option<f64> {
  let lag = params.ar_coeff_lag as i32;
  let count = params.luma_coeff_count();
  let shift = params.ar_coeff_shift as u32;
  let mut planes = Vec::new();
  if !params.scaling_points_y.is_empty() {
    planes.push((&params.ar_coeffs_y[..], 82, 73, 9..25, 57..73, 9..73));
  }
  for (coeffs, active) in [
    (
      &params.ar_coeffs_cb,
      params.chroma_scaling_from_luma || !params.scaling_points_cb.is_empty(),
    ),
    (
      &params.ar_coeffs_cr,
      params.chroma_scaling_from_luma || !params.scaling_points_cr.is_empty(),
    ),
  ] {
    // The luma coefficient feeds luma grain in and does not decide stability.
    if active {
      planes.push((&coeffs[..count.min(coeffs.len())], 44, 38, 6..14, 30..38, 6..38));
    }
  }
  let mut highest: f64 = 0.0;
  for (coeffs, width, height, top, bottom, columns) in planes {
    // A filter whose coefficients add up to less than the divisor in absolute
    // value cannot grow.
    if coeffs.iter().map(|&c| (c as i32).abs()).sum::<i32>() < 1 << shift {
      continue;
    }
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let (mut top_energy, mut bottom_energy) = (0.0, 0.0);
    for _ in 0..8 {
      let mut grain = vec![0f64; width * height];
      for value in grain.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *value = (state >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
      }
      grow(&mut grain, width, height, lag as usize, coeffs, shift);
      let energy = |rows: std::ops::Range<usize>| -> f64 {
        rows
          .flat_map(|y| columns.clone().map(move |x| (y, x)))
          .map(|(y, x)| grain[y * width + x].powi(2))
          .sum()
      };
      top_energy += energy(top.clone());
      bottom_energy += energy(bottom.clone());
    }
    let spread = (bottom_energy / top_energy).sqrt();
    highest = if spread.is_finite() { highest.max(spread) } else { f64::INFINITY };
  }
  (highest > UNSTABLE_SPREAD).then_some(highest)
}

// Runs the filter without the decoder's rounding and limits over rows 3 and
// below, columns 3 to width - 3. Every sum adds its taps in specification
// order, zero coefficients included, so the result is the same to the last bit
// as a loop pixel by pixel, but the taps of the rows above run over a whole row at a time.
fn grow(grain: &mut [f64], width: usize, height: usize, lag: usize, coeffs: &[i8], shift: u32) {
  let coeff = |pos: usize| coeffs.get(pos).copied().unwrap_or(0) as f64;
  let above: Vec<(usize, usize, f64)> = (1..=lag)
    .rev()
    .flat_map(|dy| (0..=2 * lag).map(move |dx| (dy, dx)))
    .enumerate()
    .map(|(pos, (dy, dx))| (dy, dx, coeff(pos)))
    .collect();
  let left: Vec<(usize, f64)> = (0..lag).map(|dx| (dx, coeff(above.len() + dx))).collect();
  let divisor = (1u32 << shift) as f64;
  let columns = width - 6;
  let mut sums = vec![0.0; columns];
  for y in 3..height {
    sums.fill(0.0);
    for &(dy, dx, c) in &above {
      let start = (y - dy) * width + 3 + dx - lag;
      for (sum, &value) in sums.iter_mut().zip(&grain[start..start + columns]) {
        *sum += c * value;
      }
    }
    let row = y * width;
    for (column, &sum) in sums.iter().enumerate() {
      let x = column + 3;
      let mut sum = sum;
      for &(dx, c) in &left {
        sum += c * grain[row + x + dx - lag];
      }
      grain[row + x] += sum / divisor;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::table::parse_document;

  const GOOD: &str = "filmgrn1\nE 0 100 1 7391 1\n  p 1 7 0 10 0 1 128 192 256 128 192 256\n  sY 2 0 30 255 40\n  sCb 1 128 20\n  sCr 1 128 20\n  cY 4 8 4 30\n  cCb 1 2 1 4 10\n  cCr 1 2 1 4 10\n";

  fn codes(text: &str) -> Vec<(&'static str, Severity, bool)> {
    check(&parse_document(text).unwrap(), 2, |_| {})
      .unwrap()
      .findings
      .into_iter()
      .map(|finding| (finding.code, finding.severity, finding.fixable))
      .collect()
  }

  #[test]
  fn a_valid_table_has_no_findings_and_stays_as_written() {
    let report = check(&parse_document(GOOD).unwrap(), 2, |_| {}).unwrap();
    assert!(report.findings.is_empty());
    assert_eq!(report.fixed.unwrap().to_string(), GOOD);
  }

  #[test]
  fn lists_every_error_instead_of_the_first() {
    let text = GOOD
      .replace("p 1 7 0 10", "p 1 5 0 12")
      .replace("sY 2 0 30 255 40", "sY 2 255 40 0 30");
    assert_eq!(
      codes(&text),
      [
        ("out_of_range", Severity::Error, false),
        ("out_of_range", Severity::Error, false),
        ("points_out_of_order", Severity::Error, true),
      ]
    );
    assert!(check(&parse_document(&text).unwrap(), 2, |_| {}).unwrap().fixed.is_none());
  }

  #[test]
  fn fixes_what_has_one_right_answer() {
    let text = GOOD
      .replace("sY 2 0 30 255 40", "sY 2 255 40 0 30")
      .replace("E 0 100 1 7391 1", "E 0 100 1 0 1");
    let report = check(&parse_document(&text).unwrap(), 2, |_| {}).unwrap();
    let found: Vec<_> = report.findings.iter().map(|finding| finding.code).collect();
    assert_eq!(found, ["points_out_of_order", "zero_seed"]);
    let fixed = report.fixed.unwrap();
    assert_eq!(fixed.segments[0].params.scaling_points_y, [[0, 30], [255, 40]]);
    assert_eq!(fixed.segments[0].seed, REPLACEMENT_SEED);
    assert!(GrainTable::parse(&fixed.to_string()).is_ok());
    // Points at the same brightness cannot be put in order.
    let same = GOOD.replace("sY 2 0 30 255 40", "sY 2 9 40 9 30");
    assert_eq!(codes(&same), [("points_out_of_order", Severity::Error, false)]);
  }

  #[test]
  fn warns_about_what_encoders_read_differently() {
    let commented = format!("# made by FGSEditor\n{GOOD}");
    let report = check(&parse_document(&commented).unwrap(), 2, |_| {}).unwrap();
    let first = &report.findings[0];
    assert_eq!((first.code, first.entry, first.line, first.start), ("preamble", None, 1, None));
    assert!(first.fixable);
    // The corrected table starts with filmgrn1, so every encoder reads it.
    assert_eq!(report.fixed.unwrap().to_string(), GOOD);

    let reused = format!("{GOOD}E 100 200 1 9 0\n");
    let report = check(&parse_document(&reused).unwrap(), 2, |_| {}).unwrap();
    assert_eq!(report.findings.len(), 1);
    assert_eq!((report.findings[0].code, report.findings[0].entry), ("reused_parameters", Some(2)));
    let fixed = report.fixed.unwrap().to_string();
    assert_eq!(fixed.matches("  p 1 7 0 10").count(), 2, "{fixed}");
  }

  #[test]
  fn warns_where_apply_changes_the_grain() {
    let chroma_only = GOOD.replace("sY 2 0 30 255 40", "sY 0").replace("cY 4 8 4 30", "cY");
    let report = check(&parse_document(&chroma_only).unwrap(), 2, |_| {}).unwrap();
    assert_eq!(report.findings[0].code, "chroma_without_luma");
    assert!(report.fixed.unwrap().segments[0].params.scaling_points_cb.is_empty());
    let from_luma = GOOD.replace("p 1 7 0 10 0", "p 1 7 0 10 1");
    assert_eq!(codes(&from_luma), [("chroma_points_ignored", Severity::Warning, true)]);
  }

  #[test]
  fn finds_gaps_and_overlaps_between_entries() {
    let second = |start: u64, end: u64| {
      GOOD
        .replacen("E 0 100", &format!("E {start} {end}"), 1)
        .replacen("filmgrn1\n", "", 1)
    };
    let gap = format!("{GOOD}{}", second(150, 200));
    let report = check(&parse_document(&gap).unwrap(), 2, |_| {}).unwrap();
    assert_eq!(
      (report.findings[0].code, report.findings[0].entry, report.findings[0].line),
      ("gap", Some(2), 10)
    );
    // An entry without grain marks the gap as intended.
    let marked = format!("{GOOD}E 100 150 0 0 1\n{}", second(150, 200));
    assert!(codes(&marked).is_empty());
    let overlap = format!("{GOOD}{}", second(50, 200));
    assert_eq!(codes(&overlap), [("overlap", Severity::Error, false)]);
    // An entry that ends where it starts covers nothing and does not break the run.
    let empty = format!("{GOOD}{}{}", second(100, 100), second(100, 200));
    let report = check(&parse_document(&empty).unwrap(), 2, |_| {}).unwrap();
    assert_eq!(report.findings.len(), 1);
    assert_eq!((report.findings[0].code, report.findings[0].line), ("empty_entry", 10));
    assert_eq!(report.fixed.unwrap().segments.len(), 2);
    assert_eq!(GrainTable::parse(&empty).unwrap().segments.len(), 2);
  }

  #[test]
  fn warns_about_an_unstable_filter_only() {
    // Coefficients adding up to more than the divisor make the grain grow.
    let unstable = GOOD.replace("cY 4 8 4 30", "cY 30 40 30 60");
    assert_eq!(codes(&unstable), [("unstable_filter", Severity::Warning, false)]);
    // Strong but stable grain, here 0.95 of the divisor on one tap, is fine.
    let strong = GOOD
      .replace("cY 4 8 4 30", "cY 0 0 0 122")
      .replace("cCb 1 2 1 4 10", "cCb 0 0 0 122 10");
    assert!(codes(&strong).is_empty());
  }
}
