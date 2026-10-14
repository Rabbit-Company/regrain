use crate::report::fail;
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);

// The first signal asks every loop to stop and clean up. A second one ends the
// process at once.
pub fn install() {
  let _ = ctrlc::set_handler(|| {
    if CANCELLED.swap(true, Ordering::SeqCst) {
      std::process::exit(130);
    }
  });
}

pub fn requested() -> bool {
  CANCELLED.load(Ordering::Relaxed)
}

pub fn check() -> anyhow::Result<()> {
  if requested() { Err(fail("cancelled", "cancelled")) } else { Ok(()) }
}
