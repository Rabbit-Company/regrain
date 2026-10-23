#![allow(clippy::needless_range_loop)]

mod cancel;
mod check;
mod diff;
mod generate;
mod parallel;
mod report;
mod stats;
mod stream;
mod table;
mod template;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use report::{Coded, Json, Progress, error_code, event, fail};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("REGRAIN_TARGET"), ")");

#[derive(Parser)]
#[command(
    name = "regrain",
    version = VERSION,
    about = "Measure removed film grain and put it back as AV1 film grain synthesis"
)]
struct Cli {
  #[command(subcommand)]
  command: Command,
  /// How to report progress and results
  #[arg(long, value_enum, default_value = "text", global = true)]
  progress: Progress,
  /// Print nothing except errors, the same as --progress none
  #[arg(short, long, global = true)]
  quiet: bool,
}

#[derive(Subcommand)]
enum Command {
  /// Write a film grain table into an AV1 video
  Apply {
    /// AV1 video to add grain to
    input: PathBuf,
    /// Output video
    #[arg(short, long)]
    output: PathBuf,
    /// Film grain table to apply
    #[arg(short, long)]
    table: PathBuf,
    /// Write every frame's parameters in full instead of pointing at a reference frame
    #[arg(long)]
    full_headers: bool,
    /// Give every frame of a table entry the entry's seed, so the grain pattern stands still
    #[arg(long)]
    static_grain: bool,
    /// Read the output back and decode it with dav1d before keeping it
    #[arg(long)]
    verify: bool,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    /// ffmpeg executable used for containers other than IVF
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
    /// Frame rate for streams without timestamps, such as OBU files, as 24000/1001 or 25
    #[arg(long, value_parser = parse_fps)]
    fps: Option<(u32, u32)>,
  },
  /// Measure the grain a denoiser removed and write it as a film grain table
  Diff {
    /// The original video with grain
    source: PathBuf,
    /// The denoised version of the same video, or - for y4m on standard input
    denoised: PathBuf,
    /// Output film grain table
    #[arg(short, long)]
    output: PathBuf,
    /// How much of the video to measure
    #[arg(long, value_enum, default_value = "full")]
    scan: diff::Scan,
    /// Worker threads, defaults to every available thread
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// Measure luma grain only
    #[arg(long)]
    no_chroma: bool,
    /// Area of the source that the denoised video covers, as WIDTH:HEIGHT:X:Y
    #[arg(long)]
    crop: Option<String>,
    /// Scale the source to this size after --crop, as WIDTHxHEIGHT, for a denoised video that was downscaled
    #[arg(long, value_parser = parse_resolution)]
    scale: Option<(u32, u32)>,
    /// Start measuring this many seconds into both videos
    #[arg(long, default_value_t = 0.0)]
    start: f64,
    /// Measure only this many seconds
    #[arg(long)]
    duration: Option<f64>,
    /// Brightness change between frames that starts a new scene, on a 0 to 255 scale
    #[arg(long, default_value_t = 4.0)]
    scene_threshold: f64,
    /// How alike neighbouring scenes must be to share one table entry, from 0 to 1
    #[arg(long, default_value_t = 0.08)]
    merge: f64,
    /// Print what was measured for every scene
    #[arg(short, long)]
    verbose: bool,
    /// Fail instead of warning when many frames differ between the two videos
    #[arg(long)]
    strict: bool,
    /// Bit depth of the AV1 encode the grain will be added to
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(8..=12))]
    output_depth: u8,
    /// Put back only enough grain to hide banding, at most this deviation on a 0 to 255 scale
    #[arg(long, value_name = "DEVIATION", num_args = 0..=1, default_missing_value = "0.5")]
    just_enough: Option<f64>,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    /// ffmpeg executable, ffprobe is expected next to it
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
  },
  /// Write a film grain table with grain of a chosen strength, without measuring a video
  Generate {
    /// Output film grain table
    #[arg(short, long)]
    output: PathBuf,
    /// Grain deviation on a 0 to 255 scale, a little weaker in deep shadows and highlights
    #[arg(long, required_unless_present = "iso", conflicts_with = "iso")]
    strength: Option<f64>,
    /// Camera sensor noise at this ISO instead of a fixed strength
    #[arg(long, value_parser = clap::value_parser!(u32).range(50..=409600))]
    iso: Option<u32>,
    /// Picture size the --iso noise is for, as WIDTHxHEIGHT. Smaller pictures have larger pixels and less noise
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    resolution: (u32, u32),
    /// How large the grain is
    #[arg(long, value_enum, default_value = "fine")]
    size: generate::Size,
    /// Chroma grain deviation on a 0 to 255 scale
    #[arg(long, default_value_t = 0.0)]
    chroma: f64,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
  },
  /// Read the film grain table out of an AV1 video
  Export {
    /// AV1 video to read
    input: PathBuf,
    /// Output film grain table
    #[arg(short, long)]
    output: PathBuf,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    /// ffmpeg executable used for containers other than IVF
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
    /// Frame rate for streams without timestamps, such as OBU files, as 24000/1001 or 25
    #[arg(long, value_parser = parse_fps)]
    fps: Option<(u32, u32)>,
  },
  /// Show what a film grain table holds, entry by entry
  Stats {
    /// Film grain table to describe
    table: PathBuf,
    /// Also show the grain at every brightness level
    #[arg(long)]
    curves: bool,
    /// Worker threads, defaults to every available thread
    #[arg(short = 'j', long)]
    threads: Option<usize>,
  },
  /// Check a film grain table for anything AV1 forbids or that changes the grain
  Check {
    /// Film grain table to check
    table: PathBuf,
    /// Write a corrected table here, when every error can be corrected
    #[arg(long, value_name = "OUTPUT")]
    fix: Option<PathBuf>,
    /// Overwrite the --fix output if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    /// Worker threads, defaults to every available thread
    #[arg(short = 'j', long)]
    threads: Option<usize>,
  },
  /// Print a shell completion script
  Completions {
    /// The shell to complete for
    shell: clap_complete::Shell,
  },
  /// Print the manual page in roff format
  Manual,
  /// Remove film grain synthesis from an AV1 video
  Remove {
    /// AV1 video to remove grain from
    input: PathBuf,
    /// Output video
    #[arg(short, long)]
    output: PathBuf,
    /// Read the output back and decode it with dav1d before keeping it
    #[arg(long)]
    verify: bool,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    /// ffmpeg executable used for containers other than IVF
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
    /// Frame rate for streams without timestamps, such as OBU files, as 24000/1001 or 25
    #[arg(long, value_parser = parse_fps)]
    fps: Option<(u32, u32)>,
  },
}

fn check_output(input: &Path, output: &Path, overwrite: bool) -> Result<()> {
  if output.exists() {
    if let (Ok(a), Ok(b)) = (input.canonicalize(), output.canonicalize())
      && a == b
    {
      return Err(fail("output_is_input", "the output would overwrite the input"));
    }
    if !overwrite {
      return Err(fail(
        "output_exists",
        format!("{} already exists, pass --overwrite to replace it", output.display()),
      ));
    }
  }
  Ok(())
}

// A reader that stops early, such as head, closes the pipe. That is not an error.
fn print_bytes(bytes: &[u8]) -> Result<()> {
  use std::io::Write;
  match std::io::stdout().lock().write_all(bytes) {
    Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error).code("io_error"),
    _ => Ok(()),
  }
}

fn parse_fps(value: &str) -> std::result::Result<(u32, u32), String> {
  let fraction = match value.split_once('/') {
    Some((num, den)) => num.trim().parse::<u32>().ok().zip(den.trim().parse::<u32>().ok()),
    None => value
      .trim()
      .parse::<f64>()
      .ok()
      .filter(|fps| fps.is_finite())
      .map(|fps| ((fps * 1000.0).round() as u32, 1000)),
  };
  fraction
    .filter(|&(num, den)| num > 0 && den > 0)
    .ok_or_else(|| format!("{value} is not a frame rate, use a value like 24000/1001 or 25"))
}

fn parse_resolution(value: &str) -> std::result::Result<(u32, u32), String> {
  value
    .split_once(['x', 'X', ':'])
    .and_then(|(width, height)| width.trim().parse::<u32>().ok().zip(height.trim().parse::<u32>().ok()))
    .filter(|&(width, height)| width > 0 && height > 0)
    .ok_or_else(|| format!("{value} is not a picture size, use a value like 1920x1080"))
}

fn file_size(path: &Path) -> Option<u64> {
  std::fs::metadata(path).ok().map(|meta| meta.len())
}

fn path_json(path: &Path) -> Json {
  Json::from(path.to_string_lossy().into_owned())
}

struct Reporter {
  mode: Progress,
}

impl Reporter {
  fn text(&self, line: impl AsRef<str>) {
    if self.mode == Progress::Text {
      eprintln!("{}", line.as_ref());
    }
  }

  fn summary(&self, command: &str, mut fields: Vec<(&'static str, Json)>) {
    if self.mode == Progress::Json {
      fields.insert(0, ("command", Json::from(command)));
      event("summary", fields);
    }
  }
}

// The stats summary, written entry by entry, since a table with an entry for
// every frame of a film and its curves would take gigabytes as one Json value.
fn write_stats_summary(entries: &[stats::EntryStats], average: f64) -> Result<()> {
  match stream_stats_summary(entries, average) {
    Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error).code("io_error"),
    _ => Ok(()),
  }
}

fn stream_stats_summary(entries: &[stats::EntryStats], average: f64) -> std::io::Result<()> {
  use std::io::Write;
  let curve = |values: &[f64; 256]| Json::Array(values.iter().map(|&value| Json::from(value)).collect());
  let mut out = std::io::BufWriter::new(std::io::stdout().lock());
  write!(
    out,
    "{{\"event\":\"summary\",\"command\":\"stats\",\"entries\":{},\"luma\":{},\"list\":[",
    Json::from(entries.len()),
    Json::from(average)
  )?;
  for (index, entry) in entries.iter().enumerate() {
    let mut fields = vec![
      ("start_seconds", Json::from(entry.start)),
      ("end_seconds", Json::from(entry.end)),
      ("seed", Json::from(entry.seed as u64)),
      ("luma_dark", Json::from(entry.luma[0])),
      ("luma_mid", Json::from(entry.luma[1])),
      ("luma_bright", Json::from(entry.luma[2])),
      ("luma", Json::from(entry.luma_mean)),
      ("cb", Json::from(entry.chroma[0])),
      ("cr", Json::from(entry.chroma[1])),
      ("lag", Json::from(entry.lag as u64)),
      ("correlation", Json::from(entry.correlation)),
    ];
    if let Some(curves) = &entry.curves {
      fields.push((
        "curves",
        Json::Object(vec![
          ("luma", curve(&curves.deviation[0])),
          ("cb", curve(&curves.deviation[1])),
          ("cr", curve(&curves.deviation[2])),
          ("luma_peak", curve(&curves.peak[0])),
          ("cb_peak", curve(&curves.peak[1])),
          ("cr_peak", curve(&curves.peak[2])),
        ]),
      ));
    }
    write!(out, "{}{}", if index > 0 { "," } else { "" }, Json::Object(fields))?;
  }
  writeln!(out, "]}}")?;
  out.flush()
}

// Progress of apply, remove and export, which go through the stream in stages.
// Each stage takes a share of the whole, so `percent` counts for the command:
// decoding with dav1d to verify takes far longer than reading and writing.
struct StreamProgress {
  mode: Progress,
  terminal: bool,
  shares: &'static [(stream::Stage, f64)],
  started: std::time::Instant,
  stage: std::cell::Cell<Option<(stream::Stage, std::time::Instant)>>,
  reported: std::cell::Cell<Option<std::time::Instant>>,
  shown: std::cell::Cell<bool>,
}

impl StreamProgress {
  fn new(reporter: &Reporter, command: &str, shares: &'static [(stream::Stage, f64)]) -> StreamProgress {
    if reporter.mode == Progress::Json {
      event("start", vec![("command", Json::from(command))]);
    }
    StreamProgress {
      mode: reporter.mode,
      terminal: std::io::stderr().is_terminal(),
      shares,
      started: std::time::Instant::now(),
      stage: std::cell::Cell::new(None),
      reported: std::cell::Cell::new(None),
      shown: std::cell::Cell::new(false),
    }
  }

  fn report(&self, stage: stream::Stage, done: u64, total: Option<u64>) {
    let now = std::time::Instant::now();
    let (stage_started, begins) = match self.stage.get() {
      Some((current, started)) if current == stage => (started, false),
      _ => {
        self.stage.set(Some((stage, now)));
        (now, true)
      }
    };
    // About twice a second. JSON also gets the start of every stage, so a program sees it
    // begin, which on a terminal would only print lines without a speed yet.
    let due = now - self.reported.get().unwrap_or(self.started) >= std::time::Duration::from_millis(500);
    if !(due || (begins && self.mode == Progress::Json)) || self.mode == Progress::None {
      return;
    }
    self.reported.set(Some(now));
    let seconds = (now - stage_started).as_secs_f64();
    let rate = if seconds > 0.0 { done as f64 / seconds } else { 0.0 };
    let remaining = total
      .filter(|_| rate > 0.0)
      .map(|total| (total.saturating_sub(done) as f64 / rate).round() as u64);
    let share = |wanted: stream::Stage| {
      let before: f64 = self
        .shares
        .iter()
        .take_while(|(stage, _)| *stage != wanted)
        .map(|(_, share)| share)
        .sum();
      let own = self.shares.iter().find(|(stage, _)| *stage == wanted).map(|(_, share)| *share);
      own.map(|own| (before, own))
    };
    let percent = share(stage).map(|(before, own)| {
      let part = total
        .filter(|&total| total > 0)
        .map_or(0.0, |total| done.min(total) as f64 / total as f64);
      before + own * part
    });
    match self.mode {
      Progress::None => {}
      Progress::Json => event(
        "progress",
        vec![
          ("stage", Json::from(stage.name())),
          ("frames", Json::from(done)),
          ("total", Json::from(total)),
          ("percent", Json::from(percent)),
          ("fps", Json::from(rate)),
          ("elapsed_seconds", Json::from(self.started.elapsed().as_secs_f64())),
          ("eta_seconds", Json::from(remaining)),
        ],
      ),
      Progress::Text => {
        let frames = match total {
          Some(total) => format!("{done}/{total} frames"),
          None => format!("{done} frames"),
        };
        let line = format!(
          "{}: {frames}{} {rate:.0} fps",
          stage.name(),
          percent.map_or_else(String::new, |percent| format!(" ({percent:.0}%)"))
        );
        if self.terminal {
          eprint!("\r{line}   ");
        } else {
          eprintln!("{line}");
        }
        self.shown.set(true);
      }
    }
  }

  // Ends the progress line before the result is printed.
  fn finish(&self) {
    if self.shown.get() && self.terminal {
      eprintln!();
    }
  }
}

const WRITE_SHARES: [(stream::Stage, f64); 2] = [(stream::Stage::Read, 50.0), (stream::Stage::Write, 50.0)];
const VERIFY_SHARES: [(stream::Stage, f64); 3] = [
  (stream::Stage::Read, 10.0),
  (stream::Stage::Write, 10.0),
  (stream::Stage::Verify, 80.0),
];

fn default_threads(threads: Option<usize>) -> usize {
  threads
    .filter(|&count| count > 0)
    .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |count| count.get()))
}

// Reports the start of a command that works through the entries of a table, and
// returns the progress callback for crate::parallel::map. A table that is done
// before the first progress report shows no progress at all.
fn entry_progress(reporter: &Reporter, command: &str, total: usize, threads: usize, started: std::time::Instant) -> impl FnMut(usize) {
  if reporter.mode == Progress::Json {
    event(
      "start",
      vec![
        ("command", Json::from(command)),
        ("entries", Json::from(total)),
        ("threads", Json::from(threads)),
      ],
    );
  }
  let mode = reporter.mode;
  let terminal = std::io::stderr().is_terminal();
  let mut shown = false;
  move |done| {
    if done < total || shown {
      report_entries(mode, done, total, started.elapsed().as_secs_f64(), terminal);
      shown = true;
    }
    if done == total && shown && terminal && mode == Progress::Text {
      eprintln!();
    }
  }
}

// Progress in entries of the table.
fn report_entries(mode: Progress, done: usize, total: usize, seconds: f64, terminal: bool) {
  let rate = if seconds > 0.0 { done as f64 / seconds } else { 0.0 };
  let remaining = (rate > 0.0).then(|| ((total - done) as f64 / rate).round() as u64);
  match mode {
    Progress::None => {}
    Progress::Json => event(
      "progress",
      vec![
        ("entries", Json::from(done)),
        ("total", Json::from(total)),
        ("percent", Json::from(done as f64 * 100.0 / total.max(1) as f64)),
        ("entries_per_second", Json::from(rate)),
        ("elapsed_seconds", Json::from(seconds)),
        ("eta_seconds", Json::from(remaining)),
      ],
    ),
    Progress::Text => {
      let line = format!(
        "{done}/{total} entries ({}%) {rate:.0} entries/s eta {}:{:02}",
        done * 100 / total.max(1),
        remaining.unwrap_or(0) / 60,
        remaining.unwrap_or(0) % 60
      );
      if terminal {
        eprint!("\r{line}   ");
      } else {
        eprintln!("{line}");
      }
    }
  }
}

fn run(command: Command, reporter: &Reporter) -> Result<()> {
  let started = std::time::Instant::now();
  match command {
    Command::Apply {
      input,
      output,
      table,
      full_headers,
      static_grain,
      verify,
      overwrite,
      ffmpeg,
      fps,
    } => {
      check_output(&input, &output, overwrite)?;
      let table = table::GrainTable::read(&table)?;
      let progress = StreamProgress::new(reporter, "apply", if verify { &VERIFY_SHARES } else { &WRITE_SHARES });
      let options = stream::ApplyOptions {
        ffmpeg: &ffmpeg,
        fps,
        compact: !full_headers,
        static_grain,
        verify,
        progress: &|stage, done, total| progress.report(stage, done, total),
      };
      let report = stream::apply(&input, &output, Some(&table), &options);
      progress.finish();
      let report = report?;
      if report.replaced {
        reporter.text("Replaced the film grain that was already in the video");
      }
      if report.empty_table {
        reporter.text("The table is empty, so the output has no film grain");
      }
      if report.verified {
        reporter.text("Verified the output: every frame carries the table's grain and dav1d decodes it without errors");
      }
      reporter.text(format!(
        "Applied {} segments to {} of {} frames, wrote {}",
        table.segments.len(),
        report.grained,
        report.frames,
        output.display()
      ));
      reporter.summary(
        "apply",
        vec![
          ("output", path_json(&output)),
          ("frames", Json::from(report.frames)),
          ("grained_frames", Json::from(report.grained)),
          ("entries", Json::from(table.segments.len())),
          ("replaced_existing_grain", Json::from(report.replaced)),
          ("empty_table", Json::from(report.empty_table)),
          ("verified", Json::from(report.verified)),
          ("input_bytes", Json::from(file_size(&input))),
          ("output_bytes", Json::from(file_size(&output))),
          ("seconds", Json::from(started.elapsed().as_secs_f64())),
        ],
      );
    }
    Command::Diff {
      source,
      denoised,
      output,
      scan,
      threads,
      no_chroma,
      crop,
      scale,
      start,
      duration,
      scene_threshold,
      merge,
      verbose,
      strict,
      output_depth,
      just_enough,
      overwrite,
      ffmpeg,
    } => {
      if just_enough.is_some_and(|cap| !cap.is_finite() || cap < 0.0) {
        return Err(fail("invalid_argument", "--just-enough must be 0 or more"));
      }
      check_output(&source, &output, overwrite)?;
      check_output(&denoised, &output, overwrite)?;
      if let (Ok(a), Ok(b)) = (source.canonicalize(), denoised.canonicalize())
        && a == b
      {
        return Err(fail("same_input", "the source and the denoised video are the same file"));
      }
      let threads = threads
        .filter(|&count| count > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |count| count.get()));
      let options = diff::DiffOptions {
        source,
        denoised,
        scan,
        threads,
        chroma: !no_chroma,
        crop,
        scale: scale.map(|(width, height)| (width as usize, height as usize)),
        start: start.max(0.0),
        duration: duration.filter(|&seconds| seconds > 0.0),
        scene_threshold,
        merge_tolerance: merge,
        ffmpeg,
        progress: reporter.mode,
        verbose,
        output_depth,
        just_enough,
      };
      let (table, report) = diff::run(&options)?;
      let differ = report.mismatched * 5 > report.frames;
      let differ_message = format!(
        "{} of {} frames differ a lot between the two videos. Check that they start on the same frame and show the same picture area",
        report.mismatched, report.frames
      );
      if strict && differ {
        return Err(fail("frames_differ", differ_message));
      }
      diff::write_table(&table, &output)?;
      let fps = report.frames as f64 / report.seconds.max(1e-9);
      reporter.text(format!(
        "Measured {} frames in {:.1} s ({fps:.1} fps), {} scenes, {} table entries, wrote {}",
        report.frames,
        report.seconds,
        report.scenes,
        table.segments.len(),
        output.display()
      ));
      let mut warnings = Vec::new();
      if differ {
        let message = differ_message;
        reporter.text(format!("Warning: {message}"));
        warnings.push(Json::Object(vec![
          ("code", Json::from("frames_differ")),
          ("message", Json::from(message)),
        ]));
      }
      if table.segments.is_empty() {
        reporter.text("No measurable grain was removed, the table is empty");
        warnings.push(Json::Object(vec![
          ("code", Json::from("no_grain")),
          ("message", Json::from("No measurable grain was removed, the table is empty")),
        ]));
      } else {
        reporter.text(format!(
          "Average luma grain deviation: {:.3} on a 0 to 255 scale",
          report.luma_sigma
        ));
      }
      reporter.summary(
        "diff",
        vec![
          ("output", path_json(&output)),
          ("frames", Json::from(report.frames)),
          ("frames_expected", Json::from(report.expected)),
          ("scenes", Json::from(report.scenes)),
          ("entries", Json::from(table.segments.len())),
          ("mismatched_frames", Json::from(report.mismatched)),
          ("luma_deviation", Json::from(report.luma_sigma)),
          ("scan", Json::from(format!("{scan:?}").to_lowercase())),
          ("threads", Json::from(report.threads)),
          ("fps", Json::from(fps)),
          ("seconds", Json::from(report.seconds)),
          (
            "timings",
            Json::Object(vec![
              ("probe_seconds", Json::from(report.probe_seconds)),
              ("measure_seconds", Json::from(report.measure_seconds)),
              ("fit_seconds", Json::from(report.fit_seconds)),
            ]),
          ),
          ("warnings", Json::Array(warnings)),
        ],
      );
    }
    Command::Stats { table, curves, threads } => {
      let table = table::GrainTable::read(&table)?;
      let threads = default_threads(threads);
      let total = table.segments.len();
      let entries = stats::table_stats(&table, curves, threads, entry_progress(reporter, "stats", total, threads, started))?;
      let covered: f64 = entries.iter().filter_map(|entry| entry.end.map(|end| end - entry.start)).sum();
      let weighted: f64 = entries
        .iter()
        .filter_map(|entry| entry.end.map(|end| (end - entry.start) * entry.luma_mean))
        .sum();
      let average = if covered > 0.0 {
        weighted / covered
      } else {
        entries.iter().map(|entry| entry.luma_mean).sum::<f64>() / entries.len().max(1) as f64
      };
      if reporter.mode == Progress::Text {
        use std::fmt::Write as _;
        let mut text = String::new();
        let _ = writeln!(
          text,
          "{:>12} {:>12} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>4} {:>6}",
          "start", "end", "dark", "mid", "bright", "luma", "cb", "cr", "lag", "corr"
        );
        for entry in &entries {
          let end = entry.end.map_or_else(|| "end".to_string(), stats::clock);
          let _ = writeln!(
            text,
            "{:>12} {:>12} {:>6.3} {:>6.3} {:>6.3} {:>6.3} {:>6.3} {:>6.3} {:>4} {:>6.2}",
            stats::clock(entry.start),
            end,
            entry.luma[0],
            entry.luma[1],
            entry.luma[2],
            entry.luma_mean,
            entry.chroma[0],
            entry.chroma[1],
            entry.lag,
            entry.correlation
          );
        }
        let _ = writeln!(
          text,
          "{} entries, average luma grain {average:.3} on a 0 to 255 scale. Dark, mid and bright are brightness 32, 128 and 224, corr is how alike neighbouring grain pixels are.",
          entries.len()
        );
        if curves {
          for (number, entry) in entries.iter().enumerate() {
            let Some(curves) = &entry.curves else { continue };
            let end = entry.end.map_or_else(|| "end".to_string(), stats::clock);
            let _ = writeln!(
              text,
              "\nEntry {}, {} to {}, seed {}\n{:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
              number + 1,
              stats::clock(entry.start),
              end,
              entry.seed,
              "level",
              "luma",
              "cb",
              "cr",
              "peak y",
              "cb",
              "cr"
            );
            for level in (0..256).step_by(16).chain([255]) {
              let _ = writeln!(
                text,
                "{level:>6} {:>6.3} {:>6.3} {:>6.3} {:>6.2} {:>6.2} {:>6.2}",
                curves.deviation[0][level],
                curves.deviation[1][level],
                curves.deviation[2][level],
                curves.peak[0][level],
                curves.peak[1][level],
                curves.peak[2][level]
              );
            }
          }
        }
        print_bytes(text.as_bytes())?;
      }
      if reporter.mode == Progress::Json {
        write_stats_summary(&entries, average)?;
      }
    }
    Command::Check {
      table,
      fix,
      overwrite,
      threads,
    } => {
      let text = std::fs::read_to_string(&table)
        .with_context(|| format!("failed to read {}", table.display()))
        .code("table_not_found")?;
      let document = table::parse_document(&text)
        .with_context(|| format!("invalid film grain table {}", table.display()))
        .code("invalid_table")?;
      let threads = default_threads(threads);
      let total = document.entries.len();
      let report = check::check(&document, threads, entry_progress(reporter, "check", total, threads, started))?;
      for finding in &report.findings {
        if reporter.mode == Progress::Json {
          event(
            "finding",
            vec![
              ("severity", Json::from(finding.severity.name())),
              ("code", Json::from(finding.code)),
              ("entry", Json::from(finding.entry)),
              ("line", Json::from(finding.line)),
              ("start_seconds", Json::from(finding.start_seconds())),
              ("message", Json::from(finding.message.as_str())),
              ("fixable", Json::from(finding.fixable)),
            ],
          );
        }
        let place = match (finding.entry, finding.start_seconds()) {
          (Some(entry), Some(start)) => format!("entry {entry} on line {} at {}", finding.line, stats::clock(start)),
          _ => format!("line {}", finding.line),
        };
        reporter.text(format!(
          "{}: {place}: {}{}",
          finding.severity.name(),
          finding.message,
          if fix.is_some() && finding.fixable { " (fixed)" } else { "" }
        ));
      }
      let errors = report.count(check::Severity::Error);
      let warnings = report.count(check::Severity::Warning);
      if let Some(output) = &fix {
        let Some(fixed) = &report.fixed else {
          let unfixable = report
            .findings
            .iter()
            .filter(|finding| finding.severity == check::Severity::Error && !finding.fixable)
            .count();
          return Err(fail(
            "invalid_table",
            format!("{unfixable} of the {errors} errors cannot be corrected, so nothing was written"),
          ));
        };
        check_output(&table, output, overwrite)?;
        fixed.write(output)?;
      } else if errors > 0 {
        return Err(fail(
          "invalid_table",
          format!("{errors} errors and {warnings} warnings in {}", table.display()),
        ));
      }
      reporter.text(match (&fix, errors + warnings) {
        (Some(output), _) => format!(
          "{errors} errors and {warnings} warnings in {} entries, wrote {}",
          report.entries,
          output.display()
        ),
        (None, 0) => format!("No problems in {} entries", report.entries),
        (None, _) => format!("{warnings} warnings in {} entries", report.entries),
      });
      reporter.summary(
        "check",
        vec![
          ("entries", Json::from(report.entries)),
          ("errors", Json::from(errors)),
          ("warnings", Json::from(warnings)),
          (
            "output",
            Json::from(fix.as_deref().map(|output| output.to_string_lossy().into_owned())),
          ),
        ],
      );
    }
    Command::Completions { shell } => {
      let mut command = <Cli as clap::CommandFactory>::command();
      let mut text = Vec::new();
      clap_complete::generate(shell, &mut command, "regrain", &mut text);
      print_bytes(&text)?;
    }
    Command::Manual => {
      let command = <Cli as clap::CommandFactory>::command();
      let mut text = Vec::new();
      clap_mangen::Man::new(command).render(&mut text).code("io_error")?;
      print_bytes(&text)?;
    }
    Command::Generate {
      output,
      strength,
      iso,
      resolution,
      size,
      chroma,
      overwrite,
    } => {
      if output.exists() && !overwrite {
        return Err(fail(
          "output_exists",
          format!("{} already exists, pass --overwrite to replace it", output.display()),
        ));
      }
      let model = match (strength, iso) {
        (Some(strength), _) => generate::Model::Strength(strength),
        (None, Some(iso)) => generate::Model::Iso {
          iso,
          width: resolution.0,
          height: resolution.1,
        },
        (None, None) => return Err(fail("invalid_argument", "pass --strength or --iso")),
      };
      let table = generate::generate(&generate::GenerateOptions { model, size, chroma })?;
      table.write(&output)?;
      let entry = table
        .segments
        .first()
        .map(|segment| stats::entry_stats(segment.start, segment.end, segment.seed, &segment.params, false));
      match &entry {
        Some(entry) => reporter.text(format!(
          "Luma grain {:.3} {:.3} {:.3} at brightness 32, 128 and 224, cb {:.3}, cr {:.3}, corr {:.2}, wrote {}",
          entry.luma[0],
          entry.luma[1],
          entry.luma[2],
          entry.chroma[0],
          entry.chroma[1],
          entry.correlation,
          output.display()
        )),
        None => reporter.text(format!(
          "The grain is too weak to describe, wrote an empty table to {}",
          output.display()
        )),
      }
      reporter.summary(
        "generate",
        vec![
          ("output", path_json(&output)),
          ("entries", Json::from(table.segments.len())),
          ("luma", Json::from(entry.as_ref().map(|entry| entry.luma_mean))),
          ("cb", Json::from(entry.as_ref().map(|entry| entry.chroma[0]))),
          ("cr", Json::from(entry.as_ref().map(|entry| entry.chroma[1]))),
          ("correlation", Json::from(entry.as_ref().map(|entry| entry.correlation))),
        ],
      );
    }
    Command::Export {
      input,
      output,
      overwrite,
      ffmpeg,
      fps,
    } => {
      check_output(&input, &output, overwrite)?;
      let progress = StreamProgress::new(reporter, "export", &[]);
      let exported = stream::export(&input, &ffmpeg, fps, &|stage, done, total| progress.report(stage, done, total));
      progress.finish();
      let (table, frames) = exported?;
      table.write(&output)?;
      reporter.text(format!("Wrote {} segments to {}", table.segments.len(), output.display()));
      reporter.summary(
        "export",
        vec![
          ("output", path_json(&output)),
          ("frames", Json::from(frames)),
          ("entries", Json::from(table.segments.len())),
          ("seconds", Json::from(started.elapsed().as_secs_f64())),
        ],
      );
    }
    Command::Remove {
      input,
      output,
      verify,
      overwrite,
      ffmpeg,
      fps,
    } => {
      check_output(&input, &output, overwrite)?;
      let progress = StreamProgress::new(reporter, "remove", if verify { &VERIFY_SHARES } else { &WRITE_SHARES });
      let options = stream::ApplyOptions {
        ffmpeg: &ffmpeg,
        fps,
        compact: false,
        static_grain: false,
        verify,
        progress: &|stage, done, total| progress.report(stage, done, total),
      };
      let report = stream::apply(&input, &output, None, &options);
      progress.finish();
      let report = report?;
      reporter.text(format!(
        "Removed film grain from {} frames, wrote {}",
        report.frames,
        output.display()
      ));
      reporter.summary(
        "remove",
        vec![
          ("output", path_json(&output)),
          ("frames", Json::from(report.frames)),
          ("had_grain", Json::from(report.replaced)),
          ("verified", Json::from(report.verified)),
          ("input_bytes", Json::from(file_size(&input))),
          ("output_bytes", Json::from(file_size(&output))),
          ("seconds", Json::from(started.elapsed().as_secs_f64())),
        ],
      );
    }
  }
  Ok(())
}

fn main() -> ExitCode {
  let cli = Cli::parse();
  let mode = if cli.quiet { Progress::None } else { cli.progress };
  cancel::install();
  match run(cli.command, &Reporter { mode }) {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      // Ctrl+C also reaches the ffmpeg processes, which then fail before regrain
      // notices the signal itself.
      let error = if cancel::requested() {
        fail("cancelled", "cancelled")
      } else {
        error
      };
      if mode == Progress::Json {
        event(
          "error",
          vec![
            ("code", Json::from(error_code(&error))),
            ("message", Json::from(format!("{error:#}"))),
          ],
        );
      } else {
        eprintln!("Error: {error:#}");
      }
      if error_code(&error) == "cancelled" {
        ExitCode::from(130)
      } else {
        ExitCode::FAILURE
      }
    }
  }
}
