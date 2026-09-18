//! Download progress on the terminal.

use indicatif::{ProgressBar, ProgressStyle};
use luthier_core::download::Progress;

/// A progress bar that writes to stderr, leaving stdout for results.
pub struct BarProgress {
  bar: Option<ProgressBar>,
  enabled: bool,
}

impl BarProgress {
  pub fn new(enabled: bool) -> Self {
    Self { bar: None, enabled }
  }
}

impl Progress for BarProgress {
  fn start(&mut self, label: &str, total: Option<u64>) {
    if !self.enabled {
      return;
    }
    let bar = match total {
      Some(total) => {
        let bar = ProgressBar::new(total);
        bar.set_style(
          ProgressStyle::with_template("{msg}  [{bar:30}] {bytes}/{total_bytes}  {bytes_per_sec}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
        );
        bar
      }
      None => {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
          ProgressStyle::with_template("{msg}  {spinner} {bytes}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar
      }
    };
    bar.set_message(label.to_owned());
    self.bar = Some(bar);
  }

  fn advance(&mut self, bytes: u64) {
    if let Some(bar) = &self.bar {
      bar.inc(bytes);
    }
  }

  fn finish(&mut self) {
    if let Some(bar) = self.bar.take() {
      bar.finish_and_clear();
    }
  }
}
