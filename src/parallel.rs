use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// Items a worker takes at a time.
const BATCH: usize = 64;

// Maps `items` with `f` on `threads` threads and returns the results in order.
// `progress` is called on the calling thread with the number of items done, a few
// times a second and once at the end. Stops early with an error when the run is
// cancelled.
pub fn map<T: Sync, R: Send>(
  items: &[T],
  threads: usize,
  f: impl Fn(&T) -> R + Sync,
  mut progress: impl FnMut(usize),
) -> anyhow::Result<Vec<R>> {
  let count = items.len();
  let threads = threads.clamp(1, count.div_ceil(BATCH).max(1));
  let next = AtomicUsize::new(0);
  let done = AtomicUsize::new(0);
  let mut results: Vec<Option<R>> = (0..count).map(|_| None).collect();
  std::thread::scope(|scope| {
    let workers: Vec<_> = (0..threads)
      .map(|_| {
        scope.spawn(|| {
          let mut out = Vec::new();
          loop {
            let first = next.fetch_add(BATCH, Ordering::Relaxed);
            if first >= count || crate::cancel::requested() {
              return out;
            }
            let last = (first + BATCH).min(count);
            out.extend((first..last).map(|index| (index, f(&items[index]))));
            done.fetch_add(last - first, Ordering::Relaxed);
          }
        })
      })
      .collect();
    let mut reported = Instant::now();
    while !workers.iter().all(|worker| worker.is_finished()) {
      std::thread::sleep(Duration::from_millis(10));
      if reported.elapsed() >= Duration::from_millis(250) {
        progress(done.load(Ordering::Relaxed));
        reported = Instant::now();
      }
    }
    for worker in workers {
      for (index, result) in worker.join().expect("a worker thread panicked") {
        results[index] = Some(result);
      }
    }
  });
  crate::cancel::check()?;
  progress(count);
  Ok(results.into_iter().map(|result| result.expect("every item is mapped")).collect())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keeps_the_order_and_reports_every_item() {
    let items: Vec<u64> = (0..1000).collect();
    let mut last = 0;
    let squares = map(&items, 7, |value| value * value, |done| last = done).unwrap();
    assert_eq!(squares, items.iter().map(|value| value * value).collect::<Vec<_>>());
    assert_eq!(last, 1000);
    assert!(map(&[] as &[u64], 4, |value| *value, |_| {}).unwrap().is_empty());
  }
}
