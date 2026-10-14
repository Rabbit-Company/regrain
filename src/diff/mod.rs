use crate::report::{Coded, Json, Progress, event, fail};
pub mod analyze;
pub mod decode;
pub mod model;
pub mod probe;
pub mod synth;

use crate::grain::{ChromaFormat, GrainParams};
use crate::table::{GrainTable, OPEN_END, Segment, frame_start};
use analyze::{FrameResult, Layout, Sample, Scratch, analyze};
use anyhow::{Context, Result};
use decode::{Chunk, ChunkDecoder, StreamSpec};
use model::{Fit, RunStats, fit};
use probe::{PixelFormat, ffprobe_path, probe};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PERIOD: u64 = 48;
const MIN_CHUNK: u64 = 120;
// Intra only video seeks to any frame without decoding others, so chunks can be short.
const MIN_INTRA_CHUNK: u64 = 48;
const MAX_CHUNK: u64 = 2000;
const MIN_RUN_BLOCKS: u64 = 200;
const MEMORY_BUDGET: usize = 3 << 30;
const HISTOGRAM_THRESHOLD: f64 = 0.6;
const MISMATCH_LEVELS: f64 = 2.0;
const MOTION_LEVELS: f64 = 1.0;
const SHIFT_RATIO: f64 = 0.5;
const ALIGNMENT_STRETCH: u64 = 480;
const MIN_SHIFTED_VOTES: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scan {
  /// Every frame
  Full,
  /// 24 of every 48 frames
  Half,
  /// 12 of every 48 frames
  Quarter,
  /// 6 of every 48 frames
  Eighth,
}

impl Scan {
  fn window(self) -> u64 {
    match self {
      Scan::Full => PERIOD,
      Scan::Half => PERIOD / 2,
      Scan::Quarter => PERIOD / 4,
      Scan::Eighth => PERIOD / 8,
    }
  }
}

pub struct DiffOptions {
  pub source: PathBuf,
  pub denoised: PathBuf,
  pub scan: Scan,
  pub threads: usize,
  pub chroma: bool,
  pub crop: Option<String>,
  pub start: f64,
  pub duration: Option<f64>,
  pub scene_threshold: f64,
  pub merge_tolerance: f64,
  pub ffmpeg: String,
  pub progress: Progress,
  pub verbose: bool,
  pub output_depth: u8,
}

pub struct DiffReport {
  pub frames: u64,
  pub expected: Option<u64>,
  pub mismatched: u64,
  pub scenes: usize,
  pub seconds: f64,
  pub probe_seconds: f64,
  pub measure_seconds: f64,
  pub fit_seconds: f64,
  pub luma_sigma: f64,
  pub threads: usize,
}

struct Run {
  start: u64,
  stats: RunStats,
}

enum Message {
  Frame { chunk: usize, ordinal: u64, result: FrameResult },
  Done { chunk: usize, frames: u64 },
  Failed(anyhow::Error),
}

trait FrameBuffer: Send {
  type Item: Sample;
  fn with_len(samples: usize) -> Self;
  fn bytes(&mut self) -> &mut [u8];
  fn samples(&self) -> &[Self::Item];
}

impl FrameBuffer for Vec<u8> {
  type Item = u8;

  fn with_len(samples: usize) -> Self {
    vec![0; samples]
  }

  fn bytes(&mut self) -> &mut [u8] {
    self
  }

  fn samples(&self) -> &[u8] {
    self
  }
}

impl FrameBuffer for Vec<u16> {
  type Item = u16;

  fn with_len(samples: usize) -> Self {
    vec![0; samples]
  }

  fn bytes(&mut self) -> &mut [u8] {
    // Raw video arrives as little endian 16 bit samples, which is the native layout here.
    unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr().cast::<u8>(), self.len() * 2) }
  }

  fn samples(&self) -> &[u16] {
    self
  }
}

#[cfg(target_endian = "big")]
compile_error!("regrain reads little endian raw video and needs a little endian target");

fn decode_chunk<B: FrameBuffer>(
  layout: &Layout,
  specs: &[StreamSpec; 2],
  chunk: Chunk,
  index: usize,
  stop: &AtomicBool,
  sender: &mpsc::Sender<Message>,
) -> Result<u64> {
  let mut source = ChunkDecoder::new(&specs[0], chunk);
  let mut denoised = ChunkDecoder::new(&specs[1], chunk);
  let mut source_frame = B::with_len(layout.frame_samples());
  let mut denoised_frame = B::with_len(layout.frame_samples());
  let mut scratch = Scratch::default();
  let mut ordinal = 0;
  while !stop.load(Ordering::Relaxed) {
    crate::cancel::check()?;
    let has_source = source.next_frame(source_frame.bytes())?;
    let has_denoised = denoised.next_frame(denoised_frame.bytes())?;
    if has_source != has_denoised {
      return Err(fail("frame_count_mismatch", "the videos have different frame counts"));
    }
    if !has_source {
      break;
    }
    let result = analyze(
      layout,
      chunk.frame_index(ordinal),
      source_frame.samples(),
      denoised_frame.samples(),
      &mut scratch,
    );
    if sender
      .send(Message::Frame {
        chunk: index,
        ordinal,
        result,
      })
      .is_err()
    {
      break;
    }
    ordinal += 1;
  }
  Ok(ordinal)
}

fn thumbnail_distance(a: &[u8], b: &[u8]) -> f64 {
  if a.is_empty() {
    return 0.0;
  }
  let total: u64 = a.iter().zip(b).map(|(&x, &y)| x.abs_diff(y) as u64).sum();
  total as f64 / a.len() as f64
}

// Share of the picture whose brightness distribution changed. Motion inside a
// scene moves blocks around but leaves this low.
fn histogram_distance(a: &[u8], b: &[u8]) -> f64 {
  if a.is_empty() {
    return 0.0;
  }
  let mut bins = [0i32; 32];
  for (&x, &y) in a.iter().zip(b) {
    bins[(x >> 3) as usize] += 1;
    bins[(y >> 3) as usize] -= 1;
  }
  let mut carried = 0i64;
  let mut moved = 0i64;
  for count in bins {
    carried += count as i64;
    moved += carried.abs();
  }
  moved as f64 / a.len() as f64
}

// Votes from moving frames on whether the source lines up with the denoised video,
// counted per stretch of the video.
#[derive(Default)]
struct Alignment {
  stretch: u64,
  aligned: u64,
  shifted: u64,
}

impl Alignment {
  fn vote(&mut self, index: u64, previous: &FrameResult, current: &FrameResult) -> Result<()> {
    if thumbnail_distance(&previous.thumbnail, &current.thumbnail) <= MOTION_LEVELS {
      return Ok(());
    }
    let stretch = index / ALIGNMENT_STRETCH;
    if stretch != self.stretch {
      self.check()?;
      *self = Alignment {
        stretch,
        ..Alignment::default()
      };
    }
    let aligned = thumbnail_distance(&current.source_thumbnail, &current.thumbnail)
      .max(thumbnail_distance(&previous.source_thumbnail, &previous.thumbnail));
    let behind = thumbnail_distance(&current.source_thumbnail, &previous.thumbnail);
    let ahead = thumbnail_distance(&previous.source_thumbnail, &current.thumbnail);
    if behind.min(ahead) < SHIFT_RATIO * aligned {
      self.shifted += 1;
    } else {
      self.aligned += 1;
    }
    Ok(())
  }

  fn check(&self) -> Result<()> {
    if self.shifted >= MIN_SHIFTED_VOTES && self.shifted > 2 * self.aligned {
      return Err(fail(
        "misaligned",
        format!(
          "the source and the denoised video are off by at least one frame around frame {}, check that both start on the same frame",
          self.stretch * ALIGNMENT_STRETCH
        ),
      ));
    }
    Ok(())
  }
}

struct Collector {
  planes: usize,
  threshold: f64,
  runs: Vec<Run>,
  last: Option<FrameResult>,
  frames: u64,
  mismatched: u64,
  alignment: Alignment,
}

impl Collector {
  fn add(&mut self, mut frame: FrameResult) -> Result<()> {
    let boundary = match &self.last {
      None => Some(0),
      Some(last) => {
        let gap = frame.index - last.index - 1;
        let cut = thumbnail_distance(&last.thumbnail, &frame.thumbnail) > self.threshold
          && histogram_distance(&last.thumbnail, &frame.thumbnail) > HISTOGRAM_THRESHOLD;
        if gap == 0 && !cut {
          self.alignment.vote(frame.index, last, &frame)?;
        }
        cut.then_some(frame.index - gap / 2)
      }
    };
    if let Some(start) = boundary {
      self.runs.push(Run {
        start,
        stats: RunStats::new(self.planes),
      });
    }
    let run = self.runs.last_mut().unwrap();
    for (stats, plane) in run.stats.planes.iter_mut().zip(&frame.planes) {
      stats.add(plane);
    }
    self.frames += 1;
    self.mismatched += u64::from(frame.mismatch > MISMATCH_LEVELS);
    frame.planes.clear();
    self.last = Some(frame);
    Ok(())
  }

  fn finish(&self) -> Result<()> {
    self.alignment.check()
  }
}

fn build_segments(mut runs: Vec<Run>, format: PixelFormat, tolerance: f64, output_depth: u8) -> Vec<(u64, Fit)> {
  while runs.len() > 1 {
    let Some(poor) = runs.iter().position(|run| run.stats.luma_blocks() < MIN_RUN_BLOCKS) else {
      break;
    };
    let run = runs.remove(poor);
    if poor > 0 {
      runs[poor - 1].stats.merge(&run.stats);
    } else {
      runs[0].stats.merge(&run.stats);
      runs[0].start = run.start;
    }
  }
  let mut fits: Vec<Fit> = runs.iter().map(|run| fit(&run.stats, format, output_depth)).collect();
  let mut index = 0;
  while index + 1 < runs.len() {
    if fits[index].similar(&fits[index + 1], tolerance) {
      let next = runs.remove(index + 1);
      fits.remove(index + 1);
      runs[index].stats.merge(&next.stats);
      fits[index] = fit(&runs[index].stats, format, output_depth);
    } else {
      index += 1;
    }
  }
  runs.into_iter().map(|run| run.start).zip(fits).collect()
}

fn plan_chunks(frames: Option<u64>, scan: Scan, open_ended: bool, threads: usize, min_chunk: u64) -> Vec<Chunk> {
  let window = scan.window();
  let Some(frames) = frames else {
    return vec![Chunk {
      start: 0,
      frames: None,
      period: PERIOD,
      window,
    }];
  };
  // Long chunks keep the cost of seeking into long GOPs low, four per thread keep every thread busy to the end.
  let target = (threads as u64 * 4).max(16);
  let length = (frames / target).clamp(min_chunk, MAX_CHUNK).div_ceil(PERIOD) * PERIOD;
  let mut chunks = Vec::new();
  let mut start = 0;
  while start < frames {
    let last = start + length >= frames;
    let count = if last && open_ended {
      None
    } else {
      Some(length.min(frames - start))
    };
    chunks.push(Chunk {
      start,
      frames: count,
      period: PERIOD,
      window,
    });
    start += length;
  }
  chunks
}

fn parse_crop(crop: &str) -> Result<(usize, usize)> {
  let parts: Vec<&str> = crop.split(':').collect();
  if parts.len() != 4 || parts.iter().any(|part| part.parse::<u32>().is_err()) {
    return Err(fail("invalid_argument", "crop must be WIDTH:HEIGHT:X:Y"));
  }
  Ok((parts[0].parse()?, parts[1].parse()?))
}

pub fn run(options: &DiffOptions) -> Result<(GrainTable, DiffReport)> {
  let started = Instant::now();
  let ffprobe = ffprobe_path(&options.ffmpeg);
  let from_stdin = options.denoised.as_os_str() == "-";
  if options.source.as_os_str() == "-" {
    return Err(fail("invalid_argument", "only the denoised video can come from standard input"));
  }
  let source_info = probe(&options.source, &ffprobe)?;
  let denoised_info = if from_stdin {
    probe::probe_y4m_stdin()?
  } else {
    probe(&options.denoised, &ffprobe)?
  };
  for (info, path) in [(&source_info, &options.source), (&denoised_info, &options.denoised)] {
    if path.as_os_str() == "-" {
      continue;
    }
    if info.is_variable_frame_rate() || info.has_irregular_timestamps(path, &ffprobe)? {
      return Err(fail(
        "variable_frame_rate",
        format!(
          "{} has a variable frame rate (nominal {:.3} fps, average {:.3} fps), convert it to a constant frame rate first",
          path.display(),
          info.fps(),
          info.average_fps.unwrap_or(0.0)
        ),
      ));
    }
  }
  if (source_info.fps() - denoised_info.fps()).abs() > 0.01 {
    return Err(fail(
      "frame_rate_mismatch",
      format!(
        "the videos have different frame rates ({:.3} and {:.3})",
        source_info.fps(),
        denoised_info.fps()
      ),
    ));
  }
  let (source_width, source_height) = match &options.crop {
    Some(crop) => parse_crop(crop)?,
    None => (source_info.width, source_info.height),
  };
  if (source_width, source_height) != (denoised_info.width, denoised_info.height) {
    return Err(fail(
      "size_mismatch",
      format!(
        "the source is {source_width}x{source_height} and the denoised video is {}x{}, use --crop to cut the source to the same area",
        denoised_info.width, denoised_info.height
      ),
    ));
  }
  let stream_format = PixelFormat::parse(&denoised_info.pix_fmt).code("unsupported_input")?;
  let mut format = stream_format;
  if !options.chroma {
    format.mono = true;
  }
  let layout = Layout {
    width: denoised_info.width,
    height: denoised_info.height,
    format,
  };
  if layout.width < analyze::BLOCK || layout.height < analyze::BLOCK {
    return Err(fail("unsupported_input", "the video is too small to measure grain"));
  }

  let fps = denoised_info.fps();
  let skipped = (options.start * fps).round() as u64;
  let available = [denoised_info.estimated_frames(), source_info.estimated_frames()]
    .into_iter()
    .flatten()
    .min()
    .map(|frames| frames.saturating_sub(skipped));
  let limit = options.duration.map(|seconds| (seconds * fps).round().max(1.0) as u64);
  let frames = match (available, limit) {
    (Some(a), Some(b)) => Some(a.min(b)),
    (a, b) => a.or(b),
  };
  if frames == Some(0) {
    return Err(fail("invalid_argument", "the start position is past the end of the video"));
  }
  // Standard input can only be read once from start to end, so it is one chunk.
  let chunks = if from_stdin {
    vec![Chunk {
      start: 0,
      frames: limit,
      period: PERIOD,
      window: options.scan.window(),
    }]
  } else {
    let min_chunk = if source_info.is_intra_only() && denoised_info.is_intra_only() {
      MIN_INTRA_CHUNK
    } else {
      MIN_CHUNK
    };
    plan_chunks(frames, options.scan, limit.is_none(), options.threads, min_chunk)
  };
  // Each worker holds two decoded frames and the residual of one luma plane. Very
  // large frames get fewer workers so the total stays within the budget.
  let bytes_per_sample = if format.bit_depth > 8 { 2 } else { 1 };
  let per_worker = layout.frame_samples() * bytes_per_sample * 2 + layout.width * layout.height * 2 + (16 << 20);
  let workers = options.threads.min(MEMORY_BUDGET / per_worker).clamp(1, chunks.len());
  let decode_threads = (options.threads / workers).max(1);
  let stream_layout = Layout {
    format: stream_format,
    ..layout
  };
  let spec = |path: &PathBuf, crop: Option<String>, intra_only: bool| StreamSpec {
    path: path.clone(),
    ffmpeg: options.ffmpeg.clone(),
    format: stream_format,
    crop,
    intra_only,
    fps,
    start_seconds: options.start,
    threads: decode_threads,
    frame_bytes: stream_layout.frame_samples() * bytes_per_sample,
    stdin: false,
  };
  let mut specs = [
    spec(&options.source, options.crop.clone(), source_info.is_intra_only() && !from_stdin),
    spec(&options.denoised, None, denoised_info.is_intra_only()),
  ];
  specs[1].stdin = from_stdin;
  let expected = frames.map(|total| {
    chunks
      .iter()
      .map(|chunk| {
        Chunk {
          frames: Some(chunk.frames.unwrap_or(total.saturating_sub(chunk.start))),
          ..*chunk
        }
        .selected_frames()
        .unwrap_or(0)
      })
      .sum::<u64>()
  });

  let probed = started.elapsed().as_secs_f64();
  if options.progress == Progress::Json {
    event(
      "start",
      vec![
        ("command", Json::from("diff")),
        ("frames", Json::from(expected)),
        ("width", Json::from(layout.width)),
        ("height", Json::from(layout.height)),
        ("bit_depth", Json::from(format.bit_depth as u64)),
        ("fps", Json::from(fps)),
        ("scan", Json::from(format!("{:?}", options.scan).to_lowercase())),
        ("threads", Json::from(options.threads)),
      ],
    );
  }
  let next = AtomicUsize::new(0);
  // Results of chunks ahead of the one being collected wait in memory, so workers
  // may only run a few chunks ahead.
  let collected = AtomicUsize::new(0);
  let lookahead = workers * 2;
  let stop = AtomicBool::new(false);
  let (sender, receiver) = mpsc::channel::<Message>();
  let mut collector = Collector {
    planes: layout.planes(),
    threshold: options.scene_threshold,
    runs: Vec::new(),
    last: None,
    frames: 0,
    mismatched: 0,
    alignment: Alignment::default(),
  };
  let terminal = std::io::stderr().is_terminal();

  let outcome = std::thread::scope(|scope| -> Result<()> {
    for _ in 0..workers {
      let sender = sender.clone();
      let (chunks, specs, layout, next, stop, collected) = (&chunks, &specs, &layout, &next, &stop, &collected);
      scope.spawn(move || {
        loop {
          let index = next.fetch_add(1, Ordering::Relaxed);
          if index >= chunks.len() || stop.load(Ordering::Relaxed) {
            break;
          }
          while index > collected.load(Ordering::Acquire) + lookahead && !stop.load(Ordering::Relaxed) && !crate::cancel::requested() {
            std::thread::sleep(Duration::from_millis(5));
          }
          let outcome = if layout.format.bit_depth > 8 {
            decode_chunk::<Vec<u16>>(layout, specs, chunks[index], index, stop, &sender)
          } else {
            decode_chunk::<Vec<u8>>(layout, specs, chunks[index], index, stop, &sender)
          };
          let message = match outcome {
            Ok(frames) => Message::Done { chunk: index, frames },
            Err(error) => Message::Failed(error),
          };
          if sender.send(message).is_err() {
            break;
          }
        }
      });
    }
    drop(sender);

    let mut pending: HashMap<(usize, u64), FrameResult> = HashMap::new();
    let mut done: HashMap<usize, u64> = HashMap::new();
    let (mut chunk, mut ordinal) = (0usize, 0u64);
    let mut ended = false;
    let mut received = 0u64;
    let mut last_report = started;
    let result = (|| -> Result<()> {
      for message in &receiver {
        crate::cancel::check()?;
        match message {
          Message::Failed(error) => return Err(error),
          Message::Done { chunk, frames } => {
            done.insert(chunk, frames);
          }
          Message::Frame { chunk, ordinal, result } => {
            received += 1;
            pending.insert((chunk, ordinal), result);
          }
        }
        loop {
          if let Some(result) = pending.remove(&(chunk, ordinal)) {
            if ended {
              return Err(fail("misaligned", "the videos could not be read at matching positions"));
            }
            collector.add(result)?;
            ordinal += 1;
          } else if let Some(&frames) = done.get(&chunk).filter(|&&frames| frames == ordinal) {
            if chunks[chunk].selected_frames().is_some_and(|planned| frames < planned) {
              ended = true;
            }
            chunk += 1;
            ordinal = 0;
            collected.store(chunk, Ordering::Release);
          } else {
            break;
          }
        }
        if options.progress != Progress::None && last_report.elapsed() >= Duration::from_millis(500) {
          last_report = Instant::now();
          report_progress(options.progress, received, expected, started.elapsed().as_secs_f64(), terminal);
        }
      }
      Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    drop(receiver);
    result
  });
  if options.progress != Progress::None && outcome.is_ok() {
    report_progress(
      options.progress,
      collector.frames,
      expected,
      started.elapsed().as_secs_f64(),
      terminal,
    );
    if terminal && options.progress == Progress::Text {
      eprintln!();
    }
  }
  let measured = started.elapsed().as_secs_f64();
  outcome?;
  collector.finish()?;
  if collector.frames == 0 {
    return Err(fail("no_frames", "no frames could be read from the videos"));
  }

  let analysed = collector.frames;
  let mismatched = collector.mismatched;
  let segments = build_segments(collector.runs, format, options.merge_tolerance, options.output_depth);
  let mut table = GrainTable::default();
  let mut sigma_total = 0.0;
  for (index, (start, fit)) in segments.iter().enumerate() {
    let depth_scale = (1u32 << (format.bit_depth - 8)) as f64;
    sigma_total += fit.curves[0].mean() / depth_scale;
    if options.verbose {
      let means: Vec<String> = fit
        .curves
        .iter()
        .map(|curve| format!("{:.3}", curve.mean() / depth_scale))
        .collect();
      let blocks: u64 = fit.curves[0].counts.iter().sum();
      eprintln!(
        "scene {index}: frame {start}, deviation {}, correlation {:.2} {:.2}, {blocks} blocks",
        means.join(" "),
        fit.rho[0],
        fit.rho[1]
      );
      if std::env::var_os("REGRAIN_CURVES").is_some() {
        for (plane, curve) in fit.curves.iter().enumerate() {
          let cells: Vec<String> = (0..analyze::BINS)
            .filter(|&bin| curve.counts[bin] > 0)
            .map(|bin| format!("{}:{:.2}({})", bin * 8, curve.sigma[bin], curve.counts[bin]))
            .collect();
          eprintln!("  plane {plane}: {}", cells.join(" "));
        }
      }
    }
    let Some(params) = fit
      .params
      .as_ref()
      .map(|params| finalize(params, format))
      .filter(|params| !params.is_empty())
    else {
      continue;
    };
    let end = segments.get(index + 1).map_or(OPEN_END, |(next, _)| {
      frame_start(*next, denoised_info.fps_num, denoised_info.fps_den)
    });
    table.segments.push(Segment {
      start: frame_start(*start, denoised_info.fps_num, denoised_info.fps_den),
      end,
      seed: segment_seed(index),
      params,
    });
  }
  let seconds = started.elapsed().as_secs_f64();
  let report = DiffReport {
    frames: analysed,
    expected,
    mismatched,
    probe_seconds: probed,
    measure_seconds: measured - probed,
    fit_seconds: seconds - measured,
    threads: options.threads,
    scenes: segments.len(),
    seconds,
    luma_sigma: sigma_total / segments.len().max(1) as f64,
  };
  Ok((table, report))
}

fn finalize(params: &GrainParams, format: PixelFormat) -> GrainParams {
  let chroma = ChromaFormat {
    mono_chrome: format.mono,
    subsampling_x: format.subsampling_x,
    subsampling_y: format.subsampling_y,
  };
  let mut params = params.normalized(chroma);
  let luma = params.luma_coeff_count();
  params.ar_coeffs_y.resize(luma, 0);
  params.ar_coeffs_cb.resize(luma + 1, 0);
  params.ar_coeffs_cr.resize(luma + 1, 0);
  params
}

fn segment_seed(index: usize) -> u16 {
  (index as u64).wrapping_mul(0x9e37_79b9).wrapping_add(7391) as u16
}

fn report_progress(mode: Progress, done: u64, expected: Option<u64>, seconds: f64, terminal: bool) {
  let rate = if seconds > 0.0 { done as f64 / seconds } else { 0.0 };
  let total = expected.filter(|&total| total > 0).map(|total| total.max(done));
  let remaining = total
    .filter(|_| rate > 0.0)
    .map(|total| ((total - done) as f64 / rate).round() as u64);
  match mode {
    Progress::None => {}
    Progress::Json => event(
      "progress",
      vec![
        ("frames", Json::from(done)),
        ("total", Json::from(total)),
        ("percent", Json::from(total.map(|total| done as f64 * 100.0 / total as f64))),
        ("fps", Json::from(rate)),
        ("elapsed_seconds", Json::from(seconds)),
        ("eta_seconds", Json::from(remaining)),
      ],
    ),
    Progress::Text => {
      let line = match (total, remaining) {
        (Some(total), Some(remaining)) => format!(
          "{done}/{total} frames ({}%) {rate:.1} fps eta {}:{:02}",
          done * 100 / total,
          remaining / 60,
          remaining % 60
        ),
        _ => format!("{done} frames {rate:.1} fps"),
      };
      let mut stderr = std::io::stderr().lock();
      let _ = if terminal {
        write!(stderr, "\r{line}   ")
      } else {
        writeln!(stderr, "{line}")
      };
      let _ = stderr.flush();
    }
  }
}

pub fn write_table(table: &GrainTable, path: &std::path::Path) -> Result<()> {
  table.write(path).context("failed to write the film grain table")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn plans_chunks_that_cover_every_frame() {
    let chunks = plan_chunks(Some(34310), Scan::Full, true, 32, MIN_CHUNK);
    assert_eq!(chunks[0].start, 0);
    assert_eq!(chunks[1].start, 288);
    assert!(
      chunks
        .windows(2)
        .all(|pair| pair[0].start + pair[0].frames.unwrap() == pair[1].start)
    );
    assert_eq!(chunks.last().unwrap().frames, None);
    let bounded = plan_chunks(Some(300), Scan::Quarter, false, 16, MIN_CHUNK);
    assert_eq!(bounded.len(), 3);
    assert_eq!(bounded[2].frames, Some(12));
    assert_eq!(bounded[2].selected_frames(), Some(12));
    assert_eq!(bounded[0].selected_frames(), Some(36));
    assert_eq!(plan_chunks(None, Scan::Half, true, 16, MIN_CHUNK).len(), 1);
  }

  #[test]
  fn splits_runs_at_scene_changes() {
    let mut collector = Collector {
      planes: 1,
      threshold: 8.0,
      runs: Vec::new(),
      last: None,
      frames: 0,
      mismatched: 0,
      alignment: Alignment::default(),
    };
    let frame = |index: u64, level: u8| FrameResult {
      index,
      mismatch: 0.0,
      thumbnail: vec![level; 16],
      source_thumbnail: vec![level; 16],
      planes: vec![Default::default()],
    };
    for index in 0..5 {
      collector.add(frame(index, 40)).unwrap();
    }
    for index in 5..8 {
      collector.add(frame(index, 90)).unwrap();
    }
    collector.add(frame(48, 91)).unwrap();
    collector.add(frame(96, 20)).unwrap();
    let mut panned = vec![20u8; 16];
    panned[..8].fill(60);
    collector
      .add(FrameResult {
        index: 97,
        mismatch: 0.0,
        thumbnail: panned.clone(),
        source_thumbnail: panned.clone(),
        planes: vec![Default::default()],
      })
      .unwrap();
    panned.rotate_left(8);
    collector
      .add(FrameResult {
        index: 98,
        mismatch: 3.0,
        thumbnail: panned.clone(),
        source_thumbnail: panned,
        planes: vec![Default::default()],
      })
      .unwrap();
    let starts: Vec<u64> = collector.runs.iter().map(|run| run.start).collect();
    assert_eq!(starts, vec![0, 5, 73, 97]);
    assert_eq!(collector.frames, 12);
    assert_eq!(collector.mismatched, 1);
  }

  fn moving(index: u64, offset: i64) -> FrameResult {
    let picture = |frame: i64| -> Vec<u8> { (0..64).map(|cell| ((cell * 7 + frame * 23) % 200) as u8).collect() };
    FrameResult {
      index,
      mismatch: 0.0,
      thumbnail: picture(index as i64),
      source_thumbnail: picture(index as i64 + offset),
      planes: vec![Default::default()],
    }
  }

  #[test]
  fn catches_a_frame_offset() {
    for (offset, aligned) in [(0, true), (1, false), (-1, false)] {
      let mut collector = Collector {
        planes: 1,
        threshold: 1000.0,
        runs: Vec::new(),
        last: None,
        frames: 0,
        mismatched: 0,
        alignment: Alignment::default(),
      };
      let result = (0..40)
        .try_for_each(|index| collector.add(moving(index, offset)))
        .and_then(|()| collector.finish());
      assert_eq!(result.is_ok(), aligned, "offset {offset}");
      if let Err(error) = result {
        assert_eq!(crate::report::error_code(&error), "misaligned");
      }
    }
  }

  #[test]
  fn rejects_bad_crops() {
    assert_eq!(parse_crop("1920:800:0:140").unwrap(), (1920, 800));
    assert!(parse_crop("1920x800").is_err());
  }
}
