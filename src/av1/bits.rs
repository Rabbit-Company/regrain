use anyhow::{Result, bail};

pub struct BitReader<'a> {
  data: &'a [u8],
  pos: usize,
}

impl<'a> BitReader<'a> {
  pub fn new(data: &'a [u8]) -> Self {
    Self { data, pos: 0 }
  }

  pub fn position(&self) -> usize {
    self.pos
  }

  pub fn bit(&mut self) -> Result<bool> {
    let Some(byte) = self.data.get(self.pos >> 3) else {
      bail!("unexpected end of OBU data");
    };
    let bit = (byte >> (7 - (self.pos & 7))) & 1;
    self.pos += 1;
    Ok(bit == 1)
  }

  pub fn bits(&mut self, count: u32) -> Result<u32> {
    debug_assert!(count <= 32);
    if self.pos + count as usize > self.data.len() * 8 {
      bail!("unexpected end of OBU data");
    }
    let mut value = 0u64;
    let mut remaining = count as usize;
    while remaining > 0 {
      let offset = self.pos & 7;
      let take = remaining.min(8 - offset);
      let byte = self.data[self.pos >> 3] as u64;
      let chunk = (byte >> (8 - offset - take)) & ((1 << take) - 1);
      value = (value << take) | chunk;
      self.pos += take;
      remaining -= take;
    }
    Ok(value as u32)
  }

  pub fn skip(&mut self, count: usize) -> Result<()> {
    if self.pos + count > self.data.len() * 8 {
      bail!("unexpected end of OBU data");
    }
    self.pos += count;
    Ok(())
  }

  pub fn signed(&mut self, count: u32) -> Result<i32> {
    let value = self.bits(count)? as i32;
    let sign = 1 << (count - 1);
    Ok(if value & sign != 0 { value - 2 * sign } else { value })
  }

  pub fn uvlc(&mut self) -> Result<u32> {
    let mut leading = 0;
    while !self.bit()? {
      leading += 1;
      if leading >= 32 {
        return Ok(u32::MAX);
      }
    }
    let value = self.bits(leading)?;
    Ok(value + ((1u64 << leading) - 1) as u32)
  }

  pub fn ns(&mut self, n: u32) -> Result<u32> {
    let w = 32 - n.leading_zeros();
    let m = (1u32 << w) - n;
    let v = self.bits(w - 1)?;
    if v < m {
      return Ok(v);
    }
    let extra = self.bits(1)?;
    Ok((v << 1) - m + extra)
  }
}

#[derive(Default)]
pub struct BitWriter {
  data: Vec<u8>,
  used: usize,
}

impl BitWriter {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn bit(&mut self, value: bool) {
    if self.used & 7 == 0 {
      self.data.push(0);
    }
    if value {
      let last = self.data.len() - 1;
      self.data[last] |= 1 << (7 - (self.used & 7));
    }
    self.used += 1;
  }

  pub fn bits(&mut self, value: u32, count: u32) {
    for shift in (0..count).rev() {
      self.bit((value >> shift) & 1 == 1);
    }
  }

  pub fn copy_bits(&mut self, source: &[u8], start: usize, end: usize) {
    let mut pos = start;
    while pos < end {
      if self.used & 7 == 0 && pos & 7 == 0 && end - pos >= 8 {
        let bytes = (end - pos) / 8;
        self.data.extend_from_slice(&source[pos / 8..pos / 8 + bytes]);
        self.used += bytes * 8;
        pos += bytes * 8;
      } else {
        self.bit((source[pos >> 3] >> (7 - (pos & 7))) & 1 == 1);
        pos += 1;
      }
    }
  }

  pub fn align(&mut self) {
    self.used = self.data.len() * 8;
  }

  pub fn trailing_bits(&mut self) {
    self.bit(true);
    self.align();
  }

  pub fn extend(&mut self, bytes: &[u8]) {
    debug_assert!(self.used & 7 == 0);
    self.data.extend_from_slice(bytes);
    self.used += bytes.len() * 8;
  }

  pub fn finish(self) -> Vec<u8> {
    self.data
  }
}

pub fn read_leb128(data: &[u8]) -> Result<(u64, usize)> {
  let mut value = 0u64;
  for i in 0..8 {
    let Some(&byte) = data.get(i) else {
      bail!("truncated leb128 value");
    };
    value |= ((byte & 0x7f) as u64) << (i * 7);
    if byte & 0x80 == 0 {
      return Ok((value, i + 1));
    }
  }
  bail!("invalid leb128 value");
}

pub fn write_leb128(out: &mut Vec<u8>, mut value: u64) {
  loop {
    let byte = (value & 0x7f) as u8;
    value >>= 7;
    if value == 0 {
      out.push(byte);
      return;
    }
    out.push(byte | 0x80);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reads_what_it_writes() {
    let mut writer = BitWriter::new();
    writer.bits(0b101, 3);
    writer.bits(0xabcd, 16);
    writer.bit(true);
    writer.trailing_bits();
    let data = writer.finish();
    let mut reader = BitReader::new(&data);
    assert_eq!(reader.bits(3).unwrap(), 0b101);
    assert_eq!(reader.bits(16).unwrap(), 0xabcd);
    assert!(reader.bit().unwrap());
    assert!(reader.bit().unwrap());
    assert_eq!(reader.position(), 21);
    assert_eq!(data.len(), 3);
  }

  #[test]
  fn copies_unaligned_ranges() {
    let source = [0b1011_0011, 0b0101_1100, 0xff, 0x12];
    let mut writer = BitWriter::new();
    writer.bit(false);
    writer.copy_bits(&source, 3, 29);
    let data = writer.finish();
    let mut a = BitReader::new(&source);
    a.skip(3).unwrap();
    let mut b = BitReader::new(&data);
    b.skip(1).unwrap();
    for _ in 3..29 {
      assert_eq!(a.bit().unwrap(), b.bit().unwrap());
    }
  }

  #[test]
  fn leb128_round_trip() {
    for value in [0u64, 1, 127, 128, 300, 1 << 20, (1 << 32) - 1] {
      let mut out = Vec::new();
      write_leb128(&mut out, value);
      assert_eq!(read_leb128(&out).unwrap(), (value, out.len()));
    }
  }

  #[test]
  fn signed_values() {
    let data = [0b1111_1110, 0b0000_0100];
    let mut reader = BitReader::new(&data);
    assert_eq!(reader.signed(7).unwrap(), -1);
    assert_eq!(reader.signed(7).unwrap(), 1);
  }
}
