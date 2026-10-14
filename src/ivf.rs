use crate::report::{Coded, fail};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};

pub const HEADER_SIZE: usize = 32;
const MAX_PACKET: usize = 1 << 28;

pub struct Packet {
  pub pts: u64,
  pub data: Vec<u8>,
  // OBUs per frame unit, for packets read from Annex B.
  pub units: Option<Vec<usize>>,
}

pub struct IvfReader<R: Read> {
  input: R,
  pub header: [u8; HEADER_SIZE],
  pub rate: u32,
  pub scale: u32,
}

impl<R: Read> IvfReader<R> {
  pub fn new(mut input: R) -> Result<Self> {
    let mut header = [0u8; HEADER_SIZE];
    input.read_exact(&mut header).context("failed to read the IVF header")?;
    if &header[0..4] != b"DKIF" {
      return Err(fail("unsupported_input", "not an IVF file"));
    }
    if &header[8..12] != b"AV01" {
      return Err(fail("unsupported_input", "the IVF file does not contain AV1 video"));
    }
    let header_size = u16::from_le_bytes([header[6], header[7]]) as usize;
    if header_size < HEADER_SIZE {
      bail!("invalid IVF header size");
    }
    let mut extra = vec![0u8; header_size - HEADER_SIZE];
    input.read_exact(&mut extra)?;
    header[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    let rate = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let scale = u32::from_le_bytes(header[20..24].try_into().unwrap());
    if rate == 0 || scale == 0 {
      bail!("the IVF file has no valid time base");
    }
    Ok(Self {
      input,
      header,
      rate,
      scale,
    })
  }

  pub fn next_packet(&mut self) -> Result<Option<Packet>> {
    let mut head = [0u8; 12];
    let mut filled = 0;
    while filled < head.len() {
      let read = self.input.read(&mut head[filled..]).code("io_error")?;
      if read == 0 {
        if filled == 0 {
          return Ok(None);
        }
        bail!("truncated IVF frame header");
      }
      filled += read;
    }
    let size = u32::from_le_bytes(head[0..4].try_into().unwrap()) as usize;
    if size > MAX_PACKET {
      return Err(fail(
        "invalid_bitstream",
        format!("an IVF frame claims {size} bytes, the file is damaged"),
      ));
    }
    let pts = u64::from_le_bytes(head[4..12].try_into().unwrap());
    let mut data = vec![0u8; size];
    self
      .input
      .read_exact(&mut data)
      .context("truncated IVF frame")
      .code("invalid_bitstream")?;
    Ok(Some(Packet { pts, data, units: None }))
  }
}

pub struct IvfWriter<W: Write> {
  output: W,
}

impl<W: Write> IvfWriter<W> {
  pub fn new(mut output: W, header: &[u8; HEADER_SIZE]) -> Result<Self> {
    output.write_all(header)?;
    Ok(Self { output })
  }

  pub fn write_packet(&mut self, pts: u64, data: &[u8]) -> Result<()> {
    self.output.write_all(&(data.len() as u32).to_le_bytes())?;
    self.output.write_all(&pts.to_le_bytes())?;
    self.output.write_all(data)?;
    Ok(())
  }

  pub fn finish(mut self) -> Result<W> {
    self.output.flush()?;
    Ok(self.output)
  }
}
