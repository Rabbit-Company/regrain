use super::probe::PixelFormat;
use crate::report::{Coded, fail};
use anyhow::{Context, Result};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};

#[derive(Clone, Debug)]
pub struct StreamSpec {
  pub path: PathBuf,
  pub ffmpeg: String,
  pub format: PixelFormat,
  pub crop: Option<String>,
  pub intra_only: bool,
  pub fps: f64,
  pub start_seconds: f64,
  pub threads: usize,
  pub frame_bytes: usize,
  pub stdin: bool,
}

// A stretch of frames, of which the first `window` of every `period` are read.
#[derive(Clone, Copy, Debug)]
pub struct Chunk {
  pub start: u64,
  pub frames: Option<u64>,
  pub period: u64,
  pub window: u64,
}

impl Chunk {
  pub fn is_sampled(&self) -> bool {
    self.window < self.period
  }

  pub fn frame_index(&self, ordinal: u64) -> u64 {
    if self.is_sampled() {
      self.start + ordinal / self.window * self.period + ordinal % self.window
    } else {
      self.start + ordinal
    }
  }

  pub fn selected_frames(&self) -> Option<u64> {
    let frames = self.frames?;
    if !self.is_sampled() {
      return Some(frames);
    }
    Some(frames / self.period * self.window + (frames % self.period).min(self.window))
  }
}

pub struct ChunkDecoder {
  spec: StreamSpec,
  chunk: Chunk,
  child: Option<(Child, ChildStdout)>,
  next_window: u64,
  left_in_child: Option<u64>,
  finished: bool,
  delivered: u64,
  position: u64,
  spare: Vec<u8>,
}

impl ChunkDecoder {
  pub fn new(spec: &StreamSpec, chunk: Chunk) -> ChunkDecoder {
    ChunkDecoder {
      spec: spec.clone(),
      chunk,
      child: None,
      next_window: 0,
      left_in_child: None,
      finished: false,
      delivered: 0,
      position: 0,
      spare: Vec::new(),
    }
  }

  fn spawn(&mut self, start: u64, frames: Option<u64>, select: Option<String>) -> Result<()> {
    let spec = &self.spec;
    let mut command = Command::new(&spec.ffmpeg);
    command.args(["-v", "error", "-nostdin", "-threads", &spec.threads.to_string()]);
    let seek = spec.start_seconds + if start > 0 { (start as f64 - 0.5) / spec.fps } else { 0.0 };
    if seek > 0.0 {
      command.args(["-ss", &format!("{seek:.6}")]);
    }
    command.arg("-i").arg(&spec.path).args(["-map", "0:V:0", "-an", "-sn", "-dn"]);
    if let Some(frames) = frames {
      command.args(["-frames:v", &frames.to_string()]);
    }
    let mut filters = Vec::new();
    if let Some(crop) = &spec.crop {
      filters.push(format!("crop={crop}"));
    }
    if let Some(select) = select {
      filters.push(select);
    }
    filters.push(format!("format={}", spec.format.name()));

    command.args([
      "-vf",
      &filters.join(","),
      "-fps_mode",
      "passthrough",
      "-bsf:v",
      "setts=pts=N:dts=N",
      "-f",
      "rawvideo",
      "-",
    ]);
    let mut child = command
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .spawn()
      .with_context(|| format!("failed to run {}", spec.ffmpeg))
      .code("ffmpeg_missing")?;
    let stdout = child.stdout.take().unwrap();
    self.child = Some((child, stdout));
    self.left_in_child = frames;
    Ok(())
  }

  fn close(&mut self) {
    if let Some((mut child, stdout)) = self.child.take() {
      drop(stdout);
      let _ = child.kill();
      let _ = child.wait();
    }
  }

  fn open_next(&mut self) -> Result<bool> {
    let chunk = self.chunk;
    if !chunk.is_sampled() || !self.spec.intra_only {
      if self.next_window > 0 {
        return Ok(false);
      }
      self.next_window = 1;
      let select = chunk
        .is_sampled()
        .then(|| format!("select='lt(mod(n\\,{})\\,{})'", chunk.period, chunk.window));
      self.spawn(chunk.start, chunk.selected_frames(), select)?;
      return Ok(true);
    }
    let offset = self.next_window * chunk.period;
    let frames = match chunk.frames {
      Some(total) if offset >= total => return Ok(false),
      Some(total) => chunk.window.min(total - offset),
      None => chunk.window,
    };
    self.next_window += 1;
    self.spawn(chunk.start + offset, Some(frames), None)?;
    Ok(true)
  }

  fn next_y4m_frame(&mut self, buffer: &mut [u8]) -> Result<bool> {
    let stream_bytes = self.spec.frame_bytes;
    let target = self.chunk.frame_index(self.delivered);
    if self.chunk.frames.is_some_and(|frames| target >= self.chunk.start + frames) {
      return Ok(false);
    }
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
      let mut line = Vec::new();
      if std::io::BufRead::read_until(&mut input, b'\n', &mut line).code("io_error")? == 0 {
        return Ok(false);
      }
      if !line.starts_with(b"FRAME") {
        return Err(fail("invalid_argument", "standard input is not a y4m stream"));
      }
      let keep = self.position == target;
      self.position += 1;
      let into: &mut [u8] = if keep && stream_bytes == buffer.len() {
        buffer
      } else {
        self.spare.resize(stream_bytes, 0);
        &mut self.spare
      };
      input
        .read_exact(into)
        .context("standard input ends inside a frame")
        .code("invalid_argument")?;
      if keep {
        if stream_bytes != buffer.len() {
          let length = buffer.len();
          buffer.copy_from_slice(&self.spare[..length]);
        }
        self.delivered += 1;
        return Ok(true);
      }
    }
  }

  pub fn next_frame(&mut self, buffer: &mut [u8]) -> Result<bool> {
    if self.spec.stdin {
      return self.next_y4m_frame(buffer);
    }
    let stream_bytes = self.spec.frame_bytes;
    loop {
      if self.finished {
        return Ok(false);
      }
      if self.child.is_none() && !self.open_next()? {
        self.finished = true;
        return Ok(false);
      }
      if self.left_in_child == Some(0) {
        self.close();
        continue;
      }
      let (_, stdout) = self.child.as_mut().unwrap();
      let into: &mut [u8] = if stream_bytes == buffer.len() {
        buffer
      } else {
        self.spare.resize(stream_bytes, 0);
        &mut self.spare
      };
      let mut filled = 0;
      while filled < into.len() {
        let read = stdout.read(&mut into[filled..]).context("failed to read decoded video")?;
        if read == 0 {
          break;
        }
        filled += read;
      }
      if filled == stream_bytes {
        if stream_bytes != buffer.len() {
          let length = buffer.len();
          buffer.copy_from_slice(&self.spare[..length]);
        }
        if let Some(left) = self.left_in_child.as_mut() {
          *left -= 1;
        }
        return Ok(true);
      }
      if filled != 0 {
        return Err(fail(
          "ffmpeg_failed",
          format!("ffmpeg returned a partial frame from {}", self.spec.path.display()),
        ));
      }
      let (mut child, stdout) = self.child.take().unwrap();
      drop(stdout);
      let status = child.wait()?;
      if !status.success() {
        return Err(fail(
          "ffmpeg_failed",
          format!("ffmpeg failed while decoding {} ({status})", self.spec.path.display()),
        ));
      }
      self.finished = true;
      return Ok(false);
    }
  }
}

impl Drop for ChunkDecoder {
  fn drop(&mut self) {
    self.close();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn maps_sampled_frames_to_their_positions() {
    let chunk = Chunk {
      start: 96,
      frames: Some(100),
      period: 48,
      window: 12,
    };
    assert!(chunk.is_sampled());
    assert_eq!(chunk.frame_index(0), 96);
    assert_eq!(chunk.frame_index(11), 107);
    assert_eq!(chunk.frame_index(12), 144);
    assert_eq!(chunk.frame_index(25), 193);
    assert_eq!(chunk.selected_frames(), Some(28));
    let full = Chunk {
      start: 10,
      frames: None,
      period: 48,
      window: 48,
    };
    assert_eq!(full.frame_index(70), 80);
    assert_eq!(full.selected_frames(), None);
  }
}
