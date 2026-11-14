//! Frame timing, written once a second as JSON lines when enabled.
//!
//! Only timings and counters are recorded, never session content. File writes
//! happen on their own thread, so a slow disk cannot delay a frame.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};

const REPORT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PENDING_REPORTS: usize = 16;

/// Per-pane counters for one report window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PaneCounters {
    pub(crate) updated: u32,
    pub(crate) contended: u32,
    pub(crate) max_contended_streak: u32,
}

/// Where reports go.
pub struct MetricsSink {
    tx: Sender<String>,
}

impl MetricsSink {
    /// Another way into the same file.
    pub(crate) fn lines(&self) -> Sender<String> {
        self.tx.clone()
    }

    /// Start writing reports to `path`.
    pub fn to_file(path: &Path) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let (tx, rx) = bounded::<String>(MAX_PENDING_REPORTS);
        thread::Builder::new()
            .name("metrics-writer".into())
            .spawn(move || {
                for line in rx {
                    if writeln!(file, "{line}")
                        .and_then(|()| file.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            })?;
        Ok(Self { tx })
    }
}

/// Collects one report window of frame timings.
pub(crate) struct FrameMetrics {
    sink: Option<MetricsSink>,
    window_start: Instant,
    last_frame: Option<Instant>,
    /// Whether work was already waiting when the last frame ended.
    pending_after_last: bool,
    /// All gaps between frames. Painting is event-driven, so this includes
    /// idle gaps with nothing to draw, and overstates frame cost.
    intervals_us: Vec<u32>,
    /// Gaps that began with work already waiting: pure frame pacing and cost.
    pending_intervals_us: Vec<u32>,
    /// From a session asking for a frame to that frame starting.
    wake_latency_us: Vec<u32>,
    build_us: Vec<u32>,
    cpu_us: Vec<u32>,
}

impl FrameMetrics {
    pub(crate) fn new(sink: Option<MetricsSink>) -> Self {
        Self {
            sink,
            window_start: Instant::now(),
            last_frame: None,
            pending_after_last: false,
            intervals_us: Vec::new(),
            pending_intervals_us: Vec::new(),
            wake_latency_us: Vec::new(),
            build_us: Vec::new(),
            cpu_us: Vec::new(),
        }
    }

    pub(crate) const fn sink(&self) -> Option<&MetricsSink> {
        self.sink.as_ref()
    }

    pub(crate) fn enabled(&self) -> bool {
        self.sink.is_some()
    }

    /// Record a frame. `cpu_usage` is eframe's figure for the previous frame.
    /// `wake_latency` is how long a session's request waited for this frame,
    /// and `pending_at_end` says whether another was already waiting at its end.
    pub(crate) fn record(
        &mut self,
        start: Instant,
        cpu_usage: Option<f32>,
        wake_latency: Option<Duration>,
        pending_at_end: bool,
    ) {
        if !self.enabled() {
            return;
        }
        let now = Instant::now();
        self.build_us.push(micros(now - start));
        if let Some(last) = self.last_frame {
            self.intervals_us.push(micros(start - last));
            if self.pending_after_last {
                self.pending_intervals_us.push(micros(start - last));
            }
        }
        self.last_frame = Some(start);
        self.pending_after_last = pending_at_end;
        if let Some(latency) = wake_latency {
            self.wake_latency_us.push(micros(latency));
        }
        if let Some(seconds) = cpu_usage {
            self.cpu_us.push((seconds * 1_000_000.0) as u32);
        }
    }

    /// Emit a report if the window is over, and reset `panes` when it does.
    pub(crate) fn flush_if_due(&mut self, panes: &mut [&mut PaneCounters]) {
        if self.window_start.elapsed() < REPORT_INTERVAL {
            return;
        }
        if let Some(sink) = &self.sink {
            let counters: Vec<String> = panes
                .iter()
                .map(|p| {
                    format!(
                        r#"{{"updated":{},"contended":{},"max_contended_streak":{}}}"#,
                        p.updated, p.contended, p.max_contended_streak
                    )
                })
                .collect();
            let line = format!(
                r#"{{"frames":{},"interval_us":{},"pending_frames":{},"pending_interval_us":{},"wake_latency_us":{},"build_us":{},"cpu_us":{},"panes":[{}]}}"#,
                self.build_us.len(),
                summary(&mut self.intervals_us),
                self.pending_intervals_us.len(),
                summary(&mut self.pending_intervals_us),
                summary(&mut self.wake_latency_us),
                summary(&mut self.build_us),
                summary(&mut self.cpu_us),
                counters.join(","),
            );
            // A full queue loses one report, never a frame.
            let _ = sink.tx.try_send(line);
        }
        self.intervals_us.clear();
        self.pending_intervals_us.clear();
        self.wake_latency_us.clear();
        self.build_us.clear();
        self.cpu_us.clear();
        self.window_start = Instant::now();
        for pane in panes {
            **pane = PaneCounters::default();
        }
    }
}

fn micros(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros()).unwrap_or(u32::MAX)
}

fn percentile(sorted: &[u32], percent: usize) -> u32 {
    let last = sorted.len().saturating_sub(1);
    sorted.get(last * percent / 100).copied().unwrap_or(0)
}

fn summary(samples: &mut [u32]) -> String {
    samples.sort_unstable();
    format!(
        r#"{{"p50":{},"p99":{},"max":{}}}"#,
        percentile(samples, 50),
        percentile(samples, 99),
        samples.last().copied().unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_of_no_samples_is_all_zero() {
        assert_eq!(summary(&mut []), r#"{"p50":0,"p99":0,"max":0}"#);
    }

    #[test]
    fn summary_reports_percentiles_of_unsorted_samples() {
        let mut samples: Vec<u32> = (1..=101).rev().collect();
        assert_eq!(summary(&mut samples), r#"{"p50":51,"p99":100,"max":101}"#);
    }

    #[test]
    fn disabled_metrics_collect_nothing() {
        let mut metrics = FrameMetrics::new(None);
        metrics.record(Instant::now(), Some(0.01), None, false);
        assert!(metrics.build_us.is_empty());
    }

    fn enabled_metrics() -> FrameMetrics {
        let (tx, _rx) = bounded(1);
        FrameMetrics::new(Some(MetricsSink { tx }))
    }

    #[test]
    fn an_idle_gap_counts_in_the_old_metric_and_not_in_the_new_one() {
        let mut metrics = enabled_metrics();
        let start = Instant::now();
        metrics.record(start, None, None, false);
        metrics.record(start + Duration::from_secs(2), None, None, false);
        assert_eq!(metrics.intervals_us, [2_000_000]);
        assert!(metrics.pending_intervals_us.is_empty());
    }

    #[test]
    fn a_gap_that_began_with_work_waiting_counts_in_both() {
        let mut metrics = enabled_metrics();
        let start = Instant::now();
        metrics.record(start, None, None, true);
        let next = start + Duration::from_millis(17);
        metrics.record(next, None, Some(Duration::from_millis(3)), false);
        assert_eq!(metrics.intervals_us, [17_000]);
        assert_eq!(metrics.pending_intervals_us, [17_000]);
        assert_eq!(metrics.wake_latency_us, [3_000]);
    }
}
