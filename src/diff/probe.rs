use crate::report::{Coded, fail};
use anyhow::{Context, Result, bail};
use std::io::BufRead;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
  pub bit_depth: u8,
  pub mono: bool,
  pub subsampling_x: bool,
  pub subsampling_y: bool,
}

impl PixelFormat {
  pub fn parse(name: &str) -> Result<PixelFormat> {
    let base = name.trim_end_matches("le").trim_end_matches("be");
    let (family, depth) = if let Some(depth) = base.strip_prefix("gray") {
      ("gray", depth)
    } else if base.starts_with("yuv") {
      let Some(index) = base.rfind('p') else {
        bail!("unsupported pixel format {name}");
      };
      (&base[..index], &base[index + 1..])
    } else {
      bail!("unsupported pixel format {name}");
    };
    let depth: u32 = if depth.is_empty() {
      8
    } else {
      depth.parse().with_context(|| format!("unsupported pixel format {name}"))?
    };
    let bit_depth = match depth {
      0..=8 => 8,
      9..=10 => 10,
      _ => 12,
    };
    let (mono, subsampling_x, subsampling_y) = match family {
      "gray" => (true, true, true),
      "yuv420" | "yuvj420" => (false, true, true),
      "yuv422" | "yuvj422" => (false, true, false),
      "yuv444" | "yuvj444" => (false, false, false),
      _ => bail!("unsupported pixel format {name}"),
    };
    Ok(PixelFormat {
      bit_depth,
      mono,
      subsampling_x,
      subsampling_y,
    })
  }

  pub fn name(&self) -> String {
    let suffix = match self.bit_depth {
      8 => "",
      10 => "10le",
      _ => "12le",
    };
    if self.mono {
      return format!("gray{suffix}");
    }
    let family = match (self.subsampling_x, self.subsampling_y) {
      (true, true) => "yuv420p",
      (true, false) => "yuv422p",
      _ => "yuv444p",
    };
    format!("{family}{suffix}")
  }
}

#[derive(Clone, Debug)]
pub struct VideoInfo {
  pub width: usize,
  pub height: usize,
  pub pix_fmt: String,
  pub codec: String,
  pub fps_num: u64,
  pub fps_den: u64,
  pub frames: Option<u64>,
  pub duration: Option<f64>,
  pub average_fps: Option<f64>,
}

impl VideoInfo {
  pub fn fps(&self) -> f64 {
    self.fps_num as f64 / self.fps_den as f64
  }

  // A container whose average frame rate is far from its nominal one holds frames of
  // different lengths, so seeking by time would land on the wrong frames.
  pub fn is_variable_frame_rate(&self) -> bool {
    self
      .average_fps
      .is_some_and(|average| (average - self.fps()).abs() > self.fps() * 0.01)
  }

  // Containers do not always report a variable frame rate, so the gaps between the
  // timestamps of a few hundred frames at the start and in the middle are checked too.
  pub fn has_irregular_timestamps(&self, path: &Path, ffprobe: &str) -> Result<bool> {
    let middle = self.duration.unwrap_or(0.0) / 2.0;
    let output = Command::new(ffprobe)
      .args([
        "-v",
        "error",
        "-select_streams",
        "V:0",
        "-show_entries",
        "packet=pts_time",
        "-of",
        "csv=p=0",
      ])
      .args(["-read_intervals", &format!("%+#{SAMPLE_PACKETS},{middle:.3}%+#{SAMPLE_PACKETS}")])
      .arg(path)
      .output()
      .with_context(|| format!("failed to run {ffprobe}"))
      .code("ffmpeg_missing")?;
    Ok(irregular_timestamps(&String::from_utf8_lossy(&output.stdout), self.fps()))
  }

  // Container statistics tags survive cutting and remuxing, so a tag that promises far
  // more frames than the duration holds is ignored.
  pub fn estimated_frames(&self) -> Option<u64> {
    let from_duration = self.duration.map(|seconds| (seconds * self.fps()).round() as u64);
    match (self.frames, from_duration) {
      (Some(frames), Some(limit)) if frames > limit + limit / 100 + 2 => Some(limit),
      (frames, limit) => frames.or(limit),
    }
  }

  pub fn is_intra_only(&self) -> bool {
    matches!(
      self.codec.as_str(),
      "ffv1"
        | "huffyuv"
        | "ffvhuff"
        | "utvideo"
        | "magicyuv"
        | "prores"
        | "dnxhd"
        | "mjpeg"
        | "jpeg2000"
        | "rawvideo"
        | "v210"
        | "v410"
        | "cfhd"
        | "png"
        | "lagarith"
        | "vc2"
        | "dirac"
        | "jpegxl"
        | "jpegls"
    )
  }
}

const SAMPLE_PACKETS: usize = 300;

// Both samples arrive one after the other, so a gap of more than a few frames marks
// the jump from the first to the second and is skipped.
fn irregular_timestamps(text: &str, fps: f64) -> bool {
  let mut times: Vec<f64> = text
    .lines()
    .filter_map(|line| line.trim().trim_end_matches(',').parse().ok())
    .collect();
  times.sort_by(f64::total_cmp);
  // In a short video the two samples overlap.
  times.dedup();
  let frame = 1.0 / fps;
  let mut deltas = 0usize;
  let mut irregular = 0usize;
  for pair in times.windows(2) {
    let gap = pair[1] - pair[0];
    if gap > frame * 4.0 {
      continue;
    }
    deltas += 1;
    irregular += usize::from((gap - frame).abs() > frame * 0.25);
  }
  deltas >= 20 && irregular * 50 > deltas
}

fn parse_clock(value: &str) -> Option<f64> {
  let mut seconds = 0.0;
  for part in value.split(':') {
    seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
  }
  Some(seconds)
}

pub fn ffprobe_path(ffmpeg: &str) -> String {
  let path = Path::new(ffmpeg);
  match path.file_name().and_then(|name| name.to_str()) {
    Some(name) if name.contains("ffmpeg") => path
      .with_file_name(name.replace("ffmpeg", "ffprobe"))
      .to_string_lossy()
      .into_owned(),
    _ => "ffprobe".to_string(),
  }
}

// The header of a y4m stream on standard input. The frames that follow are read by the
// decoder of the denoised video.
pub fn probe_y4m_stdin() -> Result<VideoInfo> {
  let mut line = String::new();
  std::io::stdin().lock().read_line(&mut line).code("io_error")?;
  parse_y4m_header(&line)
}

fn parse_y4m_header(line: &str) -> Result<VideoInfo> {
  let mut fields = line.trim_end().split(' ');
  if fields.next() != Some("YUV4MPEG2") {
    return Err(fail("unsupported_input", "standard input does not start with a y4m header"));
  }
  let mut info = VideoInfo {
    width: 0,
    height: 0,
    pix_fmt: "yuv420p".into(),
    codec: "rawvideo".into(),
    fps_num: 0,
    fps_den: 1,
    frames: None,
    duration: None,
    average_fps: None,
  };
  for field in fields {
    let (tag, value) = field.split_at(field.len().min(1));
    match tag {
      "W" => info.width = value.parse().unwrap_or(0),
      "H" => info.height = value.parse().unwrap_or(0),
      "F" => {
        if let Some((num, den)) = value.split_once(':') {
          info.fps_num = num.parse().unwrap_or(0);
          info.fps_den = den.parse().unwrap_or(1).max(1);
        }
      }
      "C" => {
        let (family, depth) = match value.split_once('p') {
          Some((family, depth)) if depth.chars().all(|c| c.is_ascii_digit()) && !depth.is_empty() => (family, depth),
          _ => (value, "8"),
        };
        let family = match family {
          "420jpeg" | "420paldv" | "420mpeg2" | "420" => "yuv420p",
          "422" => "yuv422p",
          "444" => "yuv444p",
          "mono" => "gray",
          other => return Err(fail("unsupported_input", format!("unsupported y4m colour space {other}"))),
        };
        info.pix_fmt = match depth {
          "8" => family.to_string(),
          depth => format!("{family}{depth}le"),
        };
      }
      _ => {}
    }
  }
  if info.width == 0 || info.height == 0 || info.fps_num == 0 {
    return Err(fail("unsupported_input", "the y4m header has no size or frame rate"));
  }
  Ok(info)
}

pub fn probe(path: &Path, ffprobe: &str) -> Result<VideoInfo> {
  if !path.is_file() {
    return Err(fail("input_not_found", format!("input file not found: {}", path.display())));
  }
  let output = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "V:0", "-show_entries"])
        .arg("stream=codec_name,width,height,pix_fmt,r_frame_rate,avg_frame_rate,nb_frames,duration:stream_tags=NUMBER_OF_FRAMES,DURATION:format=duration")
        .args(["-of", "default=nw=1"])
        .arg(path)
        .output()
        .with_context(|| format!("failed to run {ffprobe}"))
        .code("ffmpeg_missing")?;
  if !output.status.success() {
    return Err(fail(
      "unsupported_input",
      format!(
        "ffprobe could not read {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
      ),
    ));
  }
  let text = String::from_utf8_lossy(&output.stdout);
  let mut info = VideoInfo {
    width: 0,
    height: 0,
    pix_fmt: String::new(),
    codec: String::new(),
    fps_num: 0,
    fps_den: 1,
    frames: None,
    duration: None,
    average_fps: None,
  };
  for line in text.lines() {
    let Some((key, value)) = line.split_once('=') else { continue };
    let value = value.trim();
    match key.trim() {
      "codec_name" => info.codec = value.to_string(),
      "width" => info.width = value.parse().unwrap_or(0),
      "height" => info.height = value.parse().unwrap_or(0),
      "pix_fmt" => info.pix_fmt = value.to_string(),
      "avg_frame_rate" => {
        if let Some((num, den)) = value.split_once('/') {
          let (num, den): (f64, f64) = (num.parse().unwrap_or(0.0), den.parse().unwrap_or(0.0));
          info.average_fps = (num > 0.0 && den > 0.0).then(|| num / den);
        }
      }
      "r_frame_rate" => {
        if let Some((num, den)) = value.split_once('/') {
          info.fps_num = num.parse().unwrap_or(0);
          info.fps_den = den.parse().unwrap_or(1).max(1);
        }
      }
      "nb_frames" | "TAG:NUMBER_OF_FRAMES" => {
        if info.frames.is_none() {
          info.frames = value.parse().ok().filter(|&frames: &u64| frames > 0);
        }
      }
      "duration" => {
        if info.duration.is_none() {
          info.duration = value.parse().ok().filter(|&seconds: &f64| seconds > 0.0);
        }
      }
      "TAG:DURATION" if info.duration.is_none() => {
        info.duration = parse_clock(value).filter(|&seconds| seconds > 0.0);
      }
      _ => {}
    }
  }
  if info.width == 0 || info.height == 0 || info.fps_num == 0 {
    return Err(fail("unsupported_input", format!("no video stream found in {}", path.display())));
  }
  Ok(info)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn maps_pixel_formats() {
    let format = PixelFormat::parse("yuv420p10le").unwrap();
    assert_eq!(
      format,
      PixelFormat {
        bit_depth: 10,
        mono: false,
        subsampling_x: true,
        subsampling_y: true
      }
    );
    assert_eq!(format.name(), "yuv420p10le");
    assert_eq!(PixelFormat::parse("yuv444p").unwrap().name(), "yuv444p");
    assert_eq!(PixelFormat::parse("yuv422p16le").unwrap().name(), "yuv422p12le");
    assert_eq!(PixelFormat::parse("gray12le").unwrap().name(), "gray12le");
    assert_eq!(PixelFormat::parse("yuvj420p").unwrap().bit_depth, 8);
    assert!(PixelFormat::parse("rgb24").is_err());
  }

  #[test]
  fn ignores_stale_frame_tags() {
    let mut info = VideoInfo {
      width: 1440,
      height: 1080,
      pix_fmt: "yuv420p".into(),
      codec: "ffv1".into(),
      fps_num: 24000,
      fps_den: 1001,
      frames: Some(34896),
      duration: Some(120.0),
      average_fps: Some(23.976),
    };
    assert!(!info.is_variable_frame_rate());
    info.average_fps = Some(23.5);
    assert!(info.is_variable_frame_rate());
    assert_eq!(info.estimated_frames(), Some(2877));
    info.frames = Some(2877);
    assert_eq!(info.estimated_frames(), Some(2877));
    info.duration = None;
    assert_eq!(info.estimated_frames(), Some(2877));
  }

  #[test]
  fn spots_irregular_timestamps() {
    let steady: String = (0..300).map(|i| format!("{:.6}\n", i as f64 * 1001.0 / 24000.0)).collect();
    assert!(!irregular_timestamps(&steady, 24000.0 / 1001.0));
    let reordered: String = (0..300)
      .map(|i| format!("{:.3}\n", ((i / 4) * 4 + 3 - i % 4) as f64 / 25.0))
      .collect();
    assert!(!irregular_timestamps(&reordered, 25.0));
    let mixed: String = (0..300)
      .map(|i| if i < 150 { i as f64 / 30.0 } else { 5.0 + (i - 150) as f64 / 24.0 })
      .map(|time| format!("{time:.4}\n"))
      .collect();
    assert!(irregular_timestamps(&mixed, 30.0));
    let milliseconds: String = (0..300)
      .map(|i| format!("{:.3}\n", (i as f64 * 1001.0 / 24.0).round() / 1000.0))
      .collect();
    assert!(!irregular_timestamps(&milliseconds, 24000.0 / 1001.0));
    let overlapping = format!("{steady}{steady}");
    assert!(!irregular_timestamps(&overlapping, 24000.0 / 1001.0));
  }

  #[test]
  fn reads_y4m_headers() {
    let info = parse_y4m_header("YUV4MPEG2 W1440 H1080 F24000:1001 Ip A1:1 C420p10 XYSCSS=420P10\n").unwrap();
    assert_eq!((info.width, info.height, info.fps_num, info.fps_den), (1440, 1080, 24000, 1001));
    assert_eq!(PixelFormat::parse(&info.pix_fmt).unwrap().name(), "yuv420p10le");
    assert_eq!(parse_y4m_header("YUV4MPEG2 W64 H64 F25:1 C420jpeg\n").unwrap().pix_fmt, "yuv420p");
    assert_eq!(parse_y4m_header("YUV4MPEG2 W64 H64 F25:1 Cmono\n").unwrap().pix_fmt, "gray");
    assert_eq!(
      parse_y4m_header("YUV4MPEG2 W64 H64 F25:1 C444p12\n").unwrap().pix_fmt,
      "yuv444p12le"
    );
    assert!(parse_y4m_header("YUV4MPEG2 W64 H64 F25:1 C444alpha\n").is_err());
    assert!(parse_y4m_header("RIFF").is_err());
  }

  #[test]
  fn reads_clock_durations() {
    assert_eq!(parse_clock("00:23:51.013000000"), Some(1431.013));
    assert_eq!(ffprobe_path("/opt/ffmpeg/bin/ffmpeg"), "/opt/ffmpeg/bin/ffprobe");
    assert_eq!(ffprobe_path("custom"), "ffprobe");
  }
}
