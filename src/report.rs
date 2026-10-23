use std::fmt::{self, Display, Write as _};
use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Progress {
  /// Progress and results as text on standard error
  Text,
  /// One JSON object per line on standard output
  Json,
  /// Nothing except errors
  None,
}

#[derive(Debug)]
pub struct Failure {
  pub code: &'static str,
  pub message: String,
}

impl Display for Failure {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.message)
  }
}

impl std::error::Error for Failure {}

pub fn fail(code: &'static str, message: impl Display) -> anyhow::Error {
  anyhow::Error::new(Failure {
    code,
    message: message.to_string(),
  })
}

pub trait Coded<T> {
  fn code(self, code: &'static str) -> anyhow::Result<T>;
}

// Errors from rabbit-av1 carry a code of their own, as a Failure does.
fn cause_code(cause: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
  if let Some(failure) = cause.downcast_ref::<Failure>() {
    return Some(failure.code);
  }
  cause.downcast_ref::<rabbit_av1::Error>().map(rabbit_av1::Error::code)
}

impl<T, E: Into<anyhow::Error>> Coded<T> for Result<T, E> {
  fn code(self, code: &'static str) -> anyhow::Result<T> {
    self.map_err(|error| {
      let error = error.into();
      if error.chain().any(|cause| cause_code(cause).is_some()) {
        error
      } else {
        fail(code, format!("{error:#}"))
      }
    })
  }
}

pub fn error_code(error: &anyhow::Error) -> &'static str {
  error.chain().find_map(cause_code).unwrap_or("error")
}

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
  Null,
  Bool(bool),
  Int(i64),
  Float(f64),
  Str(String),
  Array(Vec<Json>),
  Object(Vec<(&'static str, Json)>),
}

impl From<bool> for Json {
  fn from(value: bool) -> Self {
    Json::Bool(value)
  }
}

impl From<u64> for Json {
  fn from(value: u64) -> Self {
    Json::Int(value as i64)
  }
}

impl From<usize> for Json {
  fn from(value: usize) -> Self {
    Json::Int(value as i64)
  }
}

impl From<f64> for Json {
  fn from(value: f64) -> Self {
    Json::Float(value)
  }
}

impl From<&str> for Json {
  fn from(value: &str) -> Self {
    Json::Str(value.to_string())
  }
}

impl From<String> for Json {
  fn from(value: String) -> Self {
    Json::Str(value)
  }
}

impl<T: Into<Json>> From<Option<T>> for Json {
  fn from(value: Option<T>) -> Self {
    value.map_or(Json::Null, Into::into)
  }
}

impl Display for Json {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Json::Null => f.write_str("null"),
      Json::Bool(value) => write!(f, "{value}"),
      Json::Int(value) => write!(f, "{value}"),
      Json::Float(value) if value.is_finite() => {
        let rounded = (value * 1000.0).round() / 1000.0;
        if rounded == rounded.trunc() && rounded.abs() < 1e15 {
          write!(f, "{rounded:.1}")
        } else {
          write!(f, "{rounded}")
        }
      }
      Json::Float(_) => f.write_str("null"),
      Json::Str(value) => {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for c in value.chars() {
          match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
              let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
          }
        }
        out.push('"');
        f.write_str(&out)
      }
      Json::Array(items) => {
        f.write_str("[")?;
        for (i, item) in items.iter().enumerate() {
          if i > 0 {
            f.write_str(",")?;
          }
          write!(f, "{item}")?;
        }
        f.write_str("]")
      }
      Json::Object(fields) => {
        f.write_str("{")?;
        for (i, (key, value)) in fields.iter().enumerate() {
          if i > 0 {
            f.write_str(",")?;
          }
          write!(f, "{}:{value}", Json::Str(key.to_string()))?;
        }
        f.write_str("}")
      }
    }
  }
}

pub fn event(name: &str, mut fields: Vec<(&'static str, Json)>) {
  fields.insert(0, ("event", Json::from(name)));
  let mut stdout = std::io::stdout().lock();
  let _ = writeln!(stdout, "{}", Json::Object(fields));
  let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn writes_valid_json() {
    let value = Json::Object(vec![
      ("text", Json::from("a \"quoted\"\\ line\nnext\u{1}")),
      ("count", Json::from(3u64)),
      ("rate", Json::from(12.34567)),
      ("whole", Json::from(5.0)),
      ("missing", Json::from(None::<u64>)),
      ("nan", Json::from(f64::NAN)),
      ("list", Json::Array(vec![Json::from(true), Json::Null])),
    ]);
    assert_eq!(
      value.to_string(),
      r#"{"text":"a \"quoted\"\\ line\nnext\u0001","count":3,"rate":12.346,"whole":5.0,"missing":null,"nan":null,"list":[true,null]}"#
    );
  }

  #[test]
  fn keeps_the_innermost_code() {
    let inner: anyhow::Result<()> = Err(fail("frame_count_mismatch", "the videos have different frame counts"));
    let wrapped = inner.code("ffmpeg_failed").map_err(|error| error.context("while measuring"));
    let error = wrapped.unwrap_err();
    assert_eq!(error_code(&error), "frame_count_mismatch");
    let plain: Result<(), std::io::Error> = Err(std::io::Error::other("disk full"));
    let error = plain.code("io_error").unwrap_err();
    assert_eq!(error_code(&error), "io_error");
    assert_eq!(error.to_string(), "disk full");
    assert_eq!(error_code(&anyhow::anyhow!("other")), "error");
  }

  #[test]
  fn keeps_rabbit_av1_codes() {
    let missing = std::fs::File::open("/does/not/exist").unwrap_err();
    let reason = missing.to_string();
    let inner: Result<(), rabbit_av1::Error> = Err(rabbit_av1::Error::from(missing).context("failed to open x.ivf"));
    let error = inner
      .code("invalid_table")
      .map_err(|error| error.context("while applying"))
      .unwrap_err();
    assert_eq!(error_code(&error), "io_error");
    let text = format!("{error:#}");
    assert!(text.starts_with("while applying: failed to open x.ivf: "), "{text}");
    assert_eq!(text.matches(reason.as_str()).count(), 1, "{text}");
  }
}
