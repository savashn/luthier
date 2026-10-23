//! Download progress on stderr, leaving stdout for results.

use crate::render::human_bytes;
use indicatif::{ProgressBar, ProgressStyle};
use luthier_core::download::Progress;

/// How far a download goes between two lines when stderr is not a
/// terminal, as a percentage of its size.
const LINE_STEP_PERCENT: u64 = 10;

/// The same, for a download whose size nobody said.
const LINE_STEP_BYTES: u64 = 256 << 20;

/// How progress is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
  /// Not at all: `--quiet` or `--json`.
  Hidden,
  /// A bar redrawn in place, at a terminal.
  Bar,
  /// A line now and then, for a log: the Home Manager module's service
  /// writes to the journal, where a bar shows nothing for the hour a
  /// sample library takes.
  Lines,
}

impl Display {
  pub fn for_stderr(quiet: bool, json: bool, terminal: bool) -> Self {
    match (quiet || json, terminal) {
      (true, _) => Display::Hidden,
      (false, true) => Display::Bar,
      (false, false) => Display::Lines,
    }
  }
}

pub struct BarProgress {
  display: Display,
  bar: Option<ProgressBar>,
  line: Option<Line>,
}

/// One download, as [`Display::Lines`] follows it.
struct Line {
  label: String,
  total: Option<u64>,
  done: u64,
  /// How many steps have been written.
  steps: u64,
}

impl Line {
  fn step(&self) -> u64 {
    match self.total {
      Some(total) if total > 0 => self.done.min(total) * 100 / total / LINE_STEP_PERCENT,
      _ => self.done / LINE_STEP_BYTES,
    }
  }

  /// Counts `bytes` more, and returns a line to write when they crossed
  /// a step: one for however many a chunk crossed, since a resumed
  /// download reports what was already there in one go.
  fn advance(&mut self, bytes: u64) -> Option<String> {
    self.done += bytes;
    let step = self.step();
    (step > self.steps).then(|| {
      self.steps = step;
      self.describe()
    })
  }

  fn describe(&self) -> String {
    match self.total {
      Some(total) if total > 0 => format!(
        "{}: {} of {} ({}%)",
        self.label,
        human_bytes(self.done),
        human_bytes(total),
        self.done.min(total) * 100 / total
      ),
      _ => format!("{}: {}", self.label, human_bytes(self.done)),
    }
  }
}

impl BarProgress {
  pub fn new(display: Display) -> Self {
    Self {
      display,
      bar: None,
      line: None,
    }
  }
}

impl Progress for BarProgress {
  fn start(&mut self, label: &str, total: Option<u64>) {
    match self.display {
      Display::Hidden => {}
      Display::Bar => self.bar = Some(bar(label, total)),
      Display::Lines => {
        match total {
          Some(total) => eprintln!("Downloading {label} ({})", human_bytes(total)),
          None => eprintln!("Downloading {label}"),
        }
        self.line = Some(Line {
          label: label.to_owned(),
          total,
          done: 0,
          steps: 0,
        });
      }
    }
  }

  fn advance(&mut self, bytes: u64) {
    if let Some(bar) = &self.bar {
      bar.inc(bytes);
    }
    if let Some(written) = self.line.as_mut().and_then(|line| line.advance(bytes)) {
      eprintln!("{written}");
    }
  }

  fn finish(&mut self) {
    if let Some(bar) = self.bar.take() {
      bar.finish_and_clear();
    }
    self.line = None;
  }
}

fn bar(label: &str, total: Option<u64>) -> ProgressBar {
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
  bar
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_line_is_written_once_per_step_crossed() {
    let mut line = Line {
      label: "kit.7z".into(),
      total: Some(1000),
      done: 0,
      steps: 0,
    };
    let written: Vec<String> = [50, 60, 300, 590]
      .into_iter()
      .filter_map(|chunk| line.advance(chunk))
      .collect();
    assert_eq!(
      written,
      [
        "kit.7z: 110 B of 1000 B (11%)",
        "kit.7z: 410 B of 1000 B (41%)",
        "kit.7z: 1000 B of 1000 B (100%)",
      ]
    );
  }
}
