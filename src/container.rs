use crate::framing::{Framing, VideoReader, detect, write_annexb, write_low_overhead};
use crate::ivf::{HEADER_SIZE, IvfWriter};
use crate::report::{Coded, fail};
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub struct Input {
  pub reader: VideoReader,
  child: Option<Child>,
}

impl Input {
  pub fn open(path: &Path, ffmpeg: &str, fps: Option<(u32, u32)>) -> Result<Input> {
    if !path.is_file() {
      return Err(fail("input_not_found", format!("input file not found: {}", path.display())));
    }
    let file = File::open(path)
      .with_context(|| format!("failed to open {}", path.display()))
      .code("io_error")?;
    let mut buffered = BufReader::with_capacity(1 << 20, file);
    let start = buffered.fill_buf().code("io_error")?;
    if detect(start).is_some() {
      return Ok(Input {
        reader: VideoReader::open(Box::new(buffered), fps)?,
        child: None,
      });
    }
    drop(buffered);
    let mut child = Command::new(ffmpeg)
      .args(["-v", "error", "-nostdin", "-i"])
      .arg(path)
      .args(["-map", "0:v:0", "-c:v", "copy", "-f", "ivf", "-"])
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .spawn()
      .with_context(|| format!("failed to run {ffmpeg}, which is needed to read {}", path.display()))
      .code("ffmpeg_missing")?;
    let stdout: Box<dyn BufRead> = Box::new(BufReader::with_capacity(1 << 20, child.stdout.take().unwrap()));
    let reader = VideoReader::open(stdout, fps)
      .with_context(|| format!("ffmpeg could not extract AV1 video from {}", path.display()))
      .code("unsupported_input")?;
    Ok(Input {
      reader,
      child: Some(child),
    })
  }

  // When reading fails while ffmpeg feeds the input, ffmpeg usually stopped first.
  pub fn explain(&mut self, error: anyhow::Error) -> anyhow::Error {
    if let Some(child) = self.child.as_mut()
      && let Ok(status) = child.wait()
      && !status.success()
    {
      return fail("ffmpeg_failed", format!("ffmpeg failed while reading the input ({status})"));
    }
    error
  }

  pub fn finish(mut self) -> Result<()> {
    drop(self.reader);
    if let Some(child) = self.child.as_mut() {
      let status = child.wait()?;
      if !status.success() {
        return Err(fail("ffmpeg_failed", format!("ffmpeg failed while reading the input ({status})")));
      }
    }
    Ok(())
  }
}

fn part_path(path: &Path, tag: &str) -> Result<PathBuf> {
  let mut name = path
    .file_stem()
    .context("output path has no file name")
    .code("invalid_argument")?
    .to_os_string();
  name.push(tag);
  if let Some(extension) = path.extension() {
    name.push(".");
    name.push(extension);
  }
  Ok(path.with_file_name(name))
}

enum Writer {
  Ivf(IvfWriter<BufWriter<File>>),
  Raw { file: BufWriter<File>, annexb: bool },
}

// Containers are muxed by ffmpeg from a finished video file, so ffmpeg can interleave
// the tracks by time instead of by how fast each one arrives.
struct Container {
  video: PathBuf,
  format: &'static str,
  source: PathBuf,
  ffmpeg: String,
}

pub struct Output {
  writer: Writer,
  container: Option<Container>,
  temp: PathBuf,
  path: PathBuf,
}

impl Output {
  pub fn create(path: &Path, source: &Path, reader: &VideoReader, ffmpeg: &str) -> Result<Output> {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("").to_ascii_lowercase();
    let temp = part_path(path, ".part")?;
    let create = |target: &Path| -> Result<BufWriter<File>> {
      let file = File::create(target)
        .with_context(|| format!("failed to create {}", target.display()))
        .code("io_error")?;
      Ok(BufWriter::with_capacity(1 << 20, file))
    };
    let ivf = |target: &Path, header: &[u8; HEADER_SIZE]| -> Result<Writer> { Ok(Writer::Ivf(IvfWriter::new(create(target)?, header)?)) };
    let (writer, container) = match extension.as_str() {
      "ivf" => (ivf(&temp, reader.ivf_header())?, None),
      "obu" => (
        Writer::Raw {
          file: create(&temp)?,
          annexb: reader.framing == Framing::AnnexB,
        },
        None,
      ),
      "mkv" | "webm" | "mp4" | "m4v" => {
        let format = match extension.as_str() {
          "mkv" => "matroska",
          "webm" => "webm",
          _ => "mp4",
        };
        let mut video = part_path(path, ".part.video")?;
        video.set_extension("ivf");
        let writer = ivf(&video, reader.ivf_header())?;
        let container = Container {
          video,
          format,
          source: source.to_path_buf(),
          ffmpeg: ffmpeg.to_string(),
        };
        (writer, Some(container))
      }
      other => return Err(fail("unsupported_output", format!("unsupported output extension: .{other}"))),
    };
    Ok(Output {
      writer,
      container,
      temp,
      path: path.to_path_buf(),
    })
  }

  pub fn write_packet(&mut self, pts: u64, data: &[u8], units: Option<&[usize]>) -> Result<()> {
    match &mut self.writer {
      Writer::Ivf(writer) => writer.write_packet(pts, data).code("io_error"),
      Writer::Raw { file, annexb: true } => {
        let units = units.ok_or_else(|| fail("unsupported_output", "Annex B output needs Annex B input"))?;
        write_annexb(file, data, units)
      }
      Writer::Raw { file, annexb: false } => write_low_overhead(file, data),
    }
  }

  fn mux(container: &Container, temp: &Path) -> Result<()> {
    let mut command = Command::new(&container.ffmpeg);
    command.args(["-v", "error", "-nostdin", "-y", "-i"]).arg(&container.video);
    let source_is_container = File::open(&container.source)
      .ok()
      .and_then(|file| BufReader::new(file).fill_buf().ok().map(|start| detect(start).is_none()))
      .unwrap_or(false);
    if source_is_container {
      command.arg("-i").arg(&container.source);
      command.args([
        "-map",
        "0:v:0",
        "-map",
        "1:a?",
        "-map",
        "1:s?",
        "-map",
        "1:t?",
        "-map_metadata",
        "1",
        "-map_chapters",
        "1",
      ]);
    } else {
      command.args(["-map", "0:v:0"]);
    }
    command
      .args(["-c", "copy", "-max_interleave_delta", "0", "-f", container.format])
      .arg(temp);
    let output = command
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .output()
      .with_context(|| format!("failed to run {}, which is needed to write {}", container.ffmpeg, temp.display()))
      .code("ffmpeg_missing")?;
    if !output.status.success() {
      let message = String::from_utf8_lossy(&output.stderr);
      return Err(fail(
        "ffmpeg_failed",
        format!(
          "ffmpeg failed while writing the output ({}): {}",
          output.status,
          message.lines().last().unwrap_or("")
        ),
      ));
    }
    Ok(())
  }

  pub fn finish(self) -> Result<Written> {
    let result = (|| -> Result<()> {
      match self.writer {
        Writer::Ivf(writer) => drop(writer.finish().code("io_error")?),
        Writer::Raw { mut file, .. } => file.flush().code("io_error")?,
      }
      if let Some(container) = &self.container {
        crate::cancel::check()?;
        Self::mux(container, &self.temp)?;
      }
      Ok(())
    })();
    if let Some(container) = &self.container {
      let _ = std::fs::remove_file(&container.video);
    }
    let written = Written {
      temp: self.temp,
      path: self.path,
    };
    match result {
      Ok(()) => Ok(written),
      Err(error) => {
        written.discard();
        Err(error)
      }
    }
  }

  pub fn discard(self) {
    drop(self.writer);
    if let Some(container) = &self.container {
      let _ = std::fs::remove_file(&container.video);
    }
    let _ = std::fs::remove_file(&self.temp);
  }
}

// A finished output that still sits under its temporary name until it is committed.
pub struct Written {
  pub temp: PathBuf,
  path: PathBuf,
}

impl Written {
  pub fn commit(self) -> Result<()> {
    let result = std::fs::rename(&self.temp, &self.path)
      .with_context(|| format!("failed to move the output to {}", self.path.display()))
      .code("io_error");
    if result.is_err() {
      let _ = std::fs::remove_file(&self.temp);
    }
    result
  }

  pub fn discard(self) {
    let _ = std::fs::remove_file(&self.temp);
  }
}
