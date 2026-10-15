use std::path::{Path, PathBuf};
use std::process::Command;

const TABLE: &str = "filmgrn1\nE 0 5000000 1 100 1\n\tp 2 7 0 10 0 1 128 192 256 128 192 256\n\tsY 3  0 20 100 60 255 25\n\tsCb 2 0 20 255 30\n\tsCr 1 128 25\n\tcY 2 4 6 4 2 4 10 24 10 4 6 26\n\tcCb 0 0 4 0 0 0 6 16 6 0 4 18 20\n\tcCr 0 0 0 0 0 0 0 0 0 0 0 0 0\nE 5000000 9223372036854775807 1 200 1\n\tp 0 6 0 11 0 1 128 192 256 128 192 256\n\tsY 1  128 40\n\tsCb 0\n\tsCr 0\n\tcY\n\tcCb 0\n\tcCr 0\n";

fn regrain(args: &[&str]) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_regrain"))
    .args(args)
    .output()
    .expect("regrain did not start")
}

fn ffmpeg(args: &[&str]) -> bool {
  Command::new("ffmpeg")
    .args(["-v", "error", "-nostdin", "-y"])
    .args(args)
    .status()
    .is_ok_and(|status| status.success())
}

fn workspace(name: &str) -> PathBuf {
  let dir = std::env::temp_dir().join(format!("regrain-test-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir
}

fn path(dir: &Path, name: &str) -> String {
  dir.join(name).to_string_lossy().into_owned()
}

fn encode_sample(dir: &Path) -> Option<String> {
  let output = path(dir, "clean.ivf");
  let source = [
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=192x128:rate=24",
    "-frames:v",
    "24",
    "-pix_fmt",
    "yuv420p10le",
  ];
  for encoder in [
    &["-c:v", "libsvtav1", "-preset", "12", "-crf", "40"][..],
    &["-c:v", "libaom-av1", "-cpu-used", "8", "-crf", "40"][..],
  ] {
    let mut args = source.to_vec();
    args.extend_from_slice(encoder);
    args.push(&output);
    if ffmpeg(&args) && Path::new(&output).metadata().is_ok_and(|meta| meta.len() > 64) {
      return Some(output);
    }
  }
  None
}

#[test]
fn apply_export_and_remove_round_trip() {
  let dir = workspace("stream");
  let Some(clean) = encode_sample(&dir) else {
    eprintln!("skipped: ffmpeg with an AV1 encoder is not available");
    return;
  };
  let table = path(&dir, "grain.tbl");
  std::fs::write(&table, TABLE).unwrap();

  for (name, extra) in [("compact.ivf", &[][..]), ("full.ivf", &["--full-headers"][..])] {
    let output = path(&dir, name);
    let mut args = vec!["apply", &clean, "-o", &output, "-t", &table];
    args.extend_from_slice(extra);
    let result = regrain(&args);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let exported = path(&dir, "exported.tbl");
    assert!(regrain(&["export", &output, "-o", &exported, "-y"]).status.success());
    assert_eq!(std::fs::read_to_string(&exported).unwrap(), TABLE);
  }
  let compact = std::fs::metadata(path(&dir, "compact.ivf")).unwrap().len();
  let full = std::fs::metadata(path(&dir, "full.ivf")).unwrap().len();
  assert!(compact < full);

  let summary = regrain(&["apply", &clean, "-o", &path(&dir, "json.ivf"), "-t", &table, "--progress", "json"]);
  let summary = String::from_utf8(summary.stdout).unwrap();
  assert!(summary.starts_with("{\"event\":\"summary\",\"command\":\"apply\""), "{summary}");
  assert!(
    summary.contains("\"frames\":24,") && summary.contains("\"entries\":2,"),
    "{summary}"
  );
  let missing = regrain(&[
    "apply",
    &clean,
    "-o",
    &path(&dir, "never.ivf"),
    "-t",
    &path(&dir, "missing.tbl"),
    "--progress",
    "json",
  ]);
  assert!(String::from_utf8_lossy(&missing.stdout).contains("\"code\":\"table_not_found\""));

  let removed = path(&dir, "removed.ivf");
  assert!(regrain(&["remove", &path(&dir, "compact.ivf"), "-o", &removed]).status.success());
  assert_eq!(std::fs::read(&removed).unwrap(), std::fs::read(&clean).unwrap());

  let refused = regrain(&["apply", &clean, "-o", &removed, "-t", &table]);
  assert!(!refused.status.success());
  assert!(String::from_utf8_lossy(&refused.stderr).contains("already exists"));

  let one_plane = path(&dir, "one-plane.tbl");
  std::fs::write(&one_plane, TABLE.replace("\tsCr 1 128 25\n", "\tsCr 0\n")).unwrap();
  let conforming = path(&dir, "one-plane.ivf");
  assert!(regrain(&["apply", &clean, "-o", &conforming, "-t", &one_plane]).status.success());
  let decoded = Command::new("ffmpeg")
    .args(["-v", "error", "-c:v", "libdav1d", "-i", &conforming, "-f", "null", "-"])
    .output();
  if let Ok(decoded) = decoded {
    assert!(decoded.stderr.is_empty(), "{}", String::from_utf8_lossy(&decoded.stderr));
  }

  let matroska = path(&dir, "grain.mkv");
  assert!(regrain(&["apply", &clean, "-o", &matroska, "-t", &table]).status.success());
  let exported = path(&dir, "matroska.tbl");
  assert!(regrain(&["export", &matroska, "-o", &exported]).status.success());
  assert_eq!(std::fs::read_to_string(&exported).unwrap().matches("\nE ").count(), 2);
  let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diff_measures_added_noise() {
  let dir = workspace("diff");
  let clean = path(&dir, "clean.mkv");
  let noisy = path(&dir, "noisy.mkv");
  let source = ["-f", "lavfi", "-i", "color=c=0x707070:size=256x192:rate=24", "-frames:v", "30"];
  let lossless = ["-pix_fmt", "yuv420p10le", "-c:v", "ffv1"];
  let mut clean_args = source.to_vec();
  clean_args.extend_from_slice(&lossless);
  clean_args.push(&clean);
  let mut noisy_args = source.to_vec();
  noisy_args.extend_from_slice(&["-vf", "format=yuv420p10le,noise=c0s=6:c0f=t"]);
  noisy_args.extend_from_slice(&lossless);
  noisy_args.push(&noisy);
  if !ffmpeg(&clean_args) || !ffmpeg(&noisy_args) {
    eprintln!("skipped: ffmpeg is not available");
    return;
  }
  let table = path(&dir, "measured.tbl");
  let result = regrain(&["diff", &noisy, &clean, "-o", &table, "-q", "--no-chroma"]);
  assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
  let text = std::fs::read_to_string(&table).unwrap();
  assert_eq!(text.matches("\nE ").count(), 1, "{text}");
  let luma = text.lines().find(|line| line.trim_start().starts_with("sY")).unwrap();
  let values: Vec<u32> = luma.split_whitespace().skip(1).map(|value| value.parse().unwrap()).collect();
  assert!(values[0] >= 1 && values[2] > 0, "{luma}");

  let with_chroma = path(&dir, "chroma.tbl");
  assert!(regrain(&["diff", &noisy, &clean, "-o", &with_chroma, "-q"]).status.success());
  let chroma_text = std::fs::read_to_string(&with_chroma).unwrap();
  let chroma_luma = chroma_text.lines().find(|line| line.trim_start().starts_with("sY")).unwrap();
  assert_eq!(luma, chroma_luma, "--no-chroma must measure the same luma");

  let capped = path(&dir, "capped.tbl");
  assert!(
    regrain(&["diff", &noisy, &clean, "-o", &capped, "-q", "--just-enough"])
      .status
      .success()
  );
  let stats = regrain(&["stats", &capped, "--progress", "json"]);
  let stats = String::from_utf8(stats.stdout).unwrap();
  assert!(stats.contains("\"luma_mid\":0.5,"), "{stats}");

  let y4m = path(&dir, "clean.y4m");
  assert!(ffmpeg(&["-i", &clean, "-strict", "-1", "-f", "yuv4mpegpipe", &y4m]));
  let piped_table = path(&dir, "piped.tbl");
  let piped = Command::new(env!("CARGO_BIN_EXE_regrain"))
    .args(["diff", &noisy, "-", "-o", &piped_table, "-q", "--no-chroma"])
    .stdin(std::fs::File::open(&y4m).unwrap())
    .output()
    .unwrap();
  assert!(piped.status.success(), "{}", String::from_utf8_lossy(&piped.stderr));
  assert_eq!(
    std::fs::read_to_string(&piped_table).unwrap(),
    std::fs::read_to_string(&table).unwrap()
  );

  let same = regrain(&["diff", &clean, &clean, "-o", &table, "-y"]);
  assert!(!same.status.success());

  let json = regrain(&["diff", &noisy, &clean, "-o", &table, "-y", "--progress", "json"]);
  assert!(json.status.success());
  let stdout = String::from_utf8(json.stdout).unwrap();
  let lines: Vec<&str> = stdout.lines().collect();
  assert!(
    lines.iter().all(|line| line.starts_with("{\"event\":") && line.ends_with('}')),
    "{stdout}"
  );
  assert!(
    lines[0].starts_with("{\"event\":\"start\",\"command\":\"diff\",\"frames\":30,"),
    "{stdout}"
  );
  let summary = lines.last().unwrap();
  assert!(summary.starts_with("{\"event\":\"summary\",\"command\":\"diff\""), "{summary}");
  for field in [
    "\"frames\":30,",
    "\"entries\":1,",
    "\"mismatched_frames\":0,",
    "\"timings\":{",
    "\"warnings\":[]",
  ] {
    assert!(summary.contains(field), "{field} missing in {summary}");
  }

  let failed = regrain(&["diff", &clean, &clean, "-o", &table, "-y", "--progress", "json"]);
  assert_eq!(failed.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&failed.stdout).starts_with("{\"event\":\"error\",\"code\":\"same_input\","));
  let exists = regrain(&["diff", &noisy, &clean, "-o", &table, "--progress", "json"]);
  assert!(String::from_utf8_lossy(&exists.stdout).contains("\"code\":\"output_exists\""));
  let quiet = regrain(&["diff", &noisy, &clean, "-o", &table, "-y", "-q"]);
  assert!(quiet.status.success() && quiet.stdout.is_empty() && quiet.stderr.is_empty());
  let empty = path(&dir, "empty.tbl");
  let copy = path(&dir, "copy.mkv");
  std::fs::copy(&clean, &copy).unwrap();
  assert!(regrain(&["diff", &copy, &clean, "-o", &empty, "-q"]).status.success());
  assert_eq!(std::fs::read_to_string(&empty).unwrap(), "filmgrn1\n");
  let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn generate_writes_the_requested_grain() {
  let dir = workspace("generate");
  let table = path(&dir, "grain.tbl");
  let made = regrain(&["generate", "-o", &table, "--strength", "1.5", "--size", "medium", "--chroma", "0.4"]);
  assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
  let stats = String::from_utf8(regrain(&["stats", &table, "--progress", "json"]).stdout).unwrap();
  for field in ["\"entries\":1,", "\"lag\":3,"] {
    assert!(stats.contains(field), "{field} missing in {stats}");
  }
  let mid: f64 = stats
    .split("\"luma_mid\":")
    .nth(1)
    .unwrap()
    .split(',')
    .next()
    .unwrap()
    .parse()
    .unwrap();
  assert!((mid - 1.5).abs() < 0.03, "{stats}");

  let exists = regrain(&["generate", "-o", &table, "--iso", "800"]);
  assert!(String::from_utf8_lossy(&exists.stderr).contains("already exists"));
  let iso = regrain(&["generate", "-o", &table, "--iso", "800", "-y", "--progress", "json"]);
  let summary = String::from_utf8(iso.stdout).unwrap();
  assert!(summary.starts_with("{\"event\":\"summary\",\"command\":\"generate\""), "{summary}");
  assert!(!regrain(&["generate", "-o", &table, "-y"]).status.success());
  assert!(
    !regrain(&["generate", "-o", &table, "-y", "--strength", "1", "--iso", "800"])
      .status
      .success()
  );
  let too_strong = regrain(&["generate", "-o", &table, "-y", "--strength", "80", "--progress", "json"]);
  assert!(String::from_utf8_lossy(&too_strong.stdout).contains("\"code\":\"invalid_argument\""));
  let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diff_leaves_letterbox_bars_without_grain() {
  let dir = workspace("bars");
  let clean = path(&dir, "clean.mkv");
  let noisy = path(&dir, "noisy.mkv");
  let source = ["-f", "lavfi", "-i", "color=c=0x707070:size=256x128:rate=24", "-frames:v", "30"];
  let lossless = ["-pix_fmt", "yuv420p10le", "-c:v", "ffv1"];
  let pad = "format=yuv420p10le,pad=256:192:0:32:black";
  let noisy_filter = format!("format=yuv420p10le,noise=c0s=6:c0f=t,{pad}");
  let mut clean_args = source.to_vec();
  clean_args.extend_from_slice(&["-vf", pad]);
  clean_args.extend_from_slice(&lossless);
  clean_args.push(&clean);
  let mut noisy_args = source.to_vec();
  noisy_args.extend_from_slice(&["-vf", &noisy_filter]);
  noisy_args.extend_from_slice(&lossless);
  noisy_args.push(&noisy);
  if !ffmpeg(&clean_args) || !ffmpeg(&noisy_args) {
    eprintln!("skipped: ffmpeg is not available");
    return;
  }
  let table = path(&dir, "bars.tbl");
  let result = regrain(&["diff", &noisy, &clean, "-o", &table, "-q", "--no-chroma"]);
  assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
  let text = std::fs::read_to_string(&table).unwrap();
  let luma = text.lines().find(|line| line.trim_start().starts_with("sY")).unwrap();
  let values: Vec<u32> = luma.split_whitespace().skip(1).map(|value| value.parse().unwrap()).collect();
  // Black is 16 on the 0 to 255 scale.
  assert_eq!(&values[..5], &[3, 16, 0, 17, values[6]], "{luma}");
  assert!(values[4] > 0, "{luma}");
  let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diff_seeks_to_matching_frames_when_the_video_starts_late() {
  let dir = workspace("late");
  let clean = path(&dir, "clean.mkv");
  let late = path(&dir, "late.mkv");
  let video = ["-f", "lavfi", "-i", "testsrc2=size=256x192:rate=24", "-frames:v", "480"];
  let mut clean_args = video.to_vec();
  clean_args.extend_from_slice(&["-pix_fmt", "yuv420p", "-c:v", "ffv1", &clean]);
  // Remuxes often hold audio from the start and the first video frame a few frames later.
  let late_args = [
    "-itsoffset",
    "0.125",
    "-i",
    &clean,
    "-f",
    "lavfi",
    "-i",
    "sine=sample_rate=48000:duration=20",
    "-map",
    "0:v",
    "-map",
    "1:a",
    "-vf",
    "noise=c0s=6:c0f=t",
    "-c:v",
    "ffv1",
    "-c:a",
    "flac",
    &late,
  ];
  if !ffmpeg(&clean_args) || !ffmpeg(&late_args) {
    eprintln!("skipped: ffmpeg is not available");
    return;
  }
  let table = path(&dir, "late.tbl");
  let result = regrain(&["diff", &late, &clean, "-o", &table, "--progress", "json", "--threads", "4"]);
  let stdout = String::from_utf8_lossy(&result.stdout);
  assert!(result.status.success(), "{stdout}");
  assert!(stdout.contains("\"mismatched_frames\":0,"), "{stdout}");
  let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn matroska_output_keeps_tracks_interleaved() {
  let dir = workspace("interleave");
  let clean = path(&dir, "clean.mkv");
  let made = ffmpeg(&[
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=192x128:rate=24",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=48000",
    "-t",
    "20",
    "-pix_fmt",
    "yuv420p",
    "-c:v",
    "libsvtav1",
    "-preset",
    "12",
    "-crf",
    "50",
    "-c:a",
    "flac",
    &clean,
  ]);
  if !made {
    eprintln!("skipped: ffmpeg with libsvtav1 is not available");
    return;
  }
  let table = path(&dir, "grain.tbl");
  std::fs::write(&table, TABLE).unwrap();
  let output = path(&dir, "grain.mkv");
  assert!(regrain(&["apply", &clean, "-o", &output, "-t", &table]).status.success());
  let packets = Command::new("ffprobe")
    .args([
      "-v",
      "error",
      "-show_entries",
      "packet=stream_index,pts_time,pos",
      "-of",
      "csv=p=0",
      &output,
    ])
    .output()
    .unwrap();
  let mut rows: Vec<(u64, usize, f64)> = String::from_utf8_lossy(&packets.stdout)
    .lines()
    .filter_map(|line| {
      let fields: Vec<&str> = line.split(',').collect();
      Some((
        fields.get(2)?.parse().ok()?,
        fields.first()?.parse().ok()?,
        fields.get(1)?.parse().ok()?,
      ))
    })
    .collect();
  rows.sort_by_key(|row| row.0);
  assert!(rows.iter().any(|row| row.1 == 1), "no audio in the output");
  // In file order, the timestamps of the two tracks may never drift far apart.
  let mut latest = [f64::NAN; 2];
  for (_, stream, time) in rows {
    latest[stream.min(1)] = time;
    if latest.iter().all(|value| value.is_finite()) {
      assert!(
        (latest[0] - latest[1]).abs() < 2.0,
        "tracks drift apart by {} s",
        (latest[0] - latest[1]).abs()
      );
    }
  }
  let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn full_disk_leaves_no_partial_output() {
  let dir = workspace("full");
  let Some(clean) = encode_sample(&dir) else {
    eprintln!("skipped: ffmpeg with an AV1 encoder is not available");
    return;
  };
  let table = path(&dir, "grain.tbl");
  std::fs::write(&table, TABLE).unwrap();
  // An ignored SIGXFSZ turns writes past the file size limit into errors, like a full disk.
  for (limit, args) in [
    ("0", vec!["export", clean.as_str(), "-o", "exported.tbl", "--progress", "json"]),
    (
      "1",
      vec![
        "apply",
        clean.as_str(),
        "-o",
        "grain.ivf",
        "-t",
        table.as_str(),
        "--progress",
        "json",
      ],
    ),
  ] {
    let script = format!("trap '' XFSZ; ulimit -f {limit}; cd \"$1\"; shift; exec \"$@\"");
    let output = Command::new("sh")
      .args(["-c", &script, "sh", &dir.to_string_lossy(), env!("CARGO_BIN_EXE_regrain")])
      .args(&args)
      .output()
      .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
      String::from_utf8_lossy(&output.stdout).contains("\"code\":\"io_error\""),
      "{}",
      String::from_utf8_lossy(&output.stdout)
    );
  }
  let left: Vec<String> = std::fs::read_dir(&dir)
    .unwrap()
    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
    .filter(|name| name.contains("part") || name == "exported.tbl" || name == "grain.ivf")
    .collect();
  assert!(left.is_empty(), "{left:?}");
  let _ = std::fs::remove_dir_all(&dir);
}
