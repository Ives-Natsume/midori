//! Worker-to-UI event plumbing.
//!
//! Events are split over two channels:
//!
//! * a control channel for low-volume events that the UI must never miss
//!   (`Connected`, `Fatal`, file/recording lifecycle, auto-tune results). It is
//!   unbounded because a blocking send on a full channel would stall the SDR
//!   step loop, and a bounded send with a timeout could still drop an event.
//!   The UI drains it every frame, so in practice it only holds a handful of
//!   entries.
//! * a bounded bulk channel for high-volume, replaceable events (plugin
//!   output, periodic VFO/Doppler updates, informational and error messages).
//!   Sending is non-blocking and drops are counted; the count is periodically
//!   reported once over the control channel so losses are never silent.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded, unbounded};
use midori_core::StreamSpec;
use midori_plugin::PluginEvent;

pub(crate) const BULK_EVENT_CAPACITY: usize = 64;
pub(crate) const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginSource {
    Cw,
    Sstv,
    Ssdv,
}

pub(crate) enum WorkerEvent {
    Connected(StreamSpec),
    Info(String),
    Error(String),
    Fatal(String),
    Doppler {
        offset_hz: f64,
        rate_hz_s: f64,
    },
    Plugin {
        source: PluginSource,
        event: PluginEvent,
    },
    VfoOffset {
        offset_hz: f64,
    },
    AutoTuneDone {
        offset_hz: Option<f64>,
    },
    FileEnded,
    RecordingStarted(PathBuf),
    RecordingError(String),
    RecordingStopped {
        path: PathBuf,
        samples: u64,
        seconds: f64,
    },
}

impl WorkerEvent {
    /// Whether this event must be delivered even when the UI falls behind.
    ///
    /// Kept as a single place to audit the classification: anything that
    /// changes connection/recording state or completes a user action is
    /// critical; high-rate telemetry and plugin chatter is bulk.
    #[cfg(test)]
    fn is_critical(&self) -> bool {
        matches!(
            self,
            WorkerEvent::Connected(_)
                | WorkerEvent::Fatal(_)
                | WorkerEvent::AutoTuneDone { .. }
                | WorkerEvent::FileEnded
                | WorkerEvent::RecordingStarted(_)
                | WorkerEvent::RecordingStopped { .. }
                | WorkerEvent::RecordingError(_)
        )
    }
}

/// Decide whether an event from the old worker should still be applied.
///
/// Once the UI has been disconnected and the worker is only shutting down, a
/// late `Connected` must not put the app back into the connected state. All
/// other events remain useful: final plugin output, recording results and log
/// messages are exactly what the user should see during shutdown.
pub(crate) fn accept_worker_event(stopping: bool, event: &WorkerEvent) -> bool {
    !(stopping && matches!(event, WorkerEvent::Connected(_)))
}

/// Producer half used by the worker thread.
#[derive(Clone)]
pub(crate) struct EventSender {
    critical: Sender<WorkerEvent>,
    bulk: Sender<WorkerEvent>,
    dropped: Arc<AtomicU64>,
    last_report: Arc<Mutex<Instant>>,
}

/// Consumer half polled by the UI thread.
pub(crate) struct EventReceiver {
    critical: Receiver<WorkerEvent>,
    bulk: Receiver<WorkerEvent>,
}

impl EventSender {
    pub(crate) fn new() -> (Self, EventReceiver) {
        let (critical_tx, critical_rx) = unbounded();
        let (bulk_tx, bulk_rx) = bounded(BULK_EVENT_CAPACITY);
        (
            Self {
                critical: critical_tx,
                bulk: bulk_tx,
                dropped: Arc::new(AtomicU64::new(0)),
                last_report: Arc::new(Mutex::new(Instant::now())),
            },
            EventReceiver {
                critical: critical_rx,
                bulk: bulk_rx,
            },
        )
    }

    /// Deliver an event that must not be lost. The control channel is
    /// unbounded, so this only fails when the UI has already gone away.
    pub(crate) fn critical(&self, event: WorkerEvent) {
        let _ = self.critical.send(event);
    }

    /// Deliver a high-volume event, dropping it if the UI is behind and
    /// reporting dropped events at most once per second.
    pub(crate) fn best_effort(&self, event: WorkerEvent) {
        self.best_effort_at(event, Instant::now());
    }

    /// Flush a pending drop report once the interval has elapsed. Called once
    /// per worker iteration so a burst that stops producing events is still
    /// reported.
    pub(crate) fn report_drops(&self) {
        self.report_drops_if_due(Instant::now());
    }

    fn best_effort_at(&self, event: WorkerEvent, now: Instant) {
        match self.bulk.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.record_drop(now),
            // The UI is gone (application shutdown); nothing to report to.
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn record_drop(&self, now: Instant) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        self.report_drops_if_due(now);
    }

    fn report_drops_if_due(&self, now: Instant) {
        let mut last_report = match self.last_report.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if now.duration_since(*last_report) < DROP_REPORT_INTERVAL {
            return;
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped == 0 {
            return;
        }
        *last_report = now;
        drop(last_report);
        self.critical(WorkerEvent::Info(format!(
            "{dropped} worker events dropped"
        )));
    }
}

impl EventReceiver {
    /// Pop one event, preferring queued critical events over bulk events.
    pub(crate) fn try_recv(&self) -> Option<WorkerEvent> {
        match self.critical.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => self.bulk.try_recv().ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midori_core::SampleFormat;

    fn info(message: &str) -> WorkerEvent {
        WorkerEvent::Info(message.to_string())
    }

    fn fill_bulk(sender: &EventSender, now: Instant) {
        for index in 0..BULK_EVENT_CAPACITY {
            sender.best_effort_at(info(&format!("bulk {index}")), now);
        }
    }

    #[test]
    fn classifies_critical_and_bulk_events() {
        let (sender, rx) = EventSender::new();
        let spec = StreamSpec {
            center_freq_hz: 100_000_000.0,
            sample_rate_hz: 1_000_000.0,
            bandwidth_hz: 1_000_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        let critical = [
            WorkerEvent::Connected(spec),
            WorkerEvent::Fatal("boom".to_string()),
            WorkerEvent::FileEnded,
            WorkerEvent::RecordingStarted(PathBuf::from("x")),
            WorkerEvent::RecordingStopped {
                path: PathBuf::from("x"),
                samples: 1,
                seconds: 0.1,
            },
            WorkerEvent::RecordingError("bad".to_string()),
            WorkerEvent::AutoTuneDone { offset_hz: None },
        ];
        for event in critical {
            assert!(event.is_critical(), "expected critical event");
        }
        for event in [
            info("hello"),
            WorkerEvent::Error("bad".to_string()),
            WorkerEvent::Doppler {
                offset_hz: 0.0,
                rate_hz_s: 0.0,
            },
            WorkerEvent::VfoOffset { offset_hz: 0.0 },
            WorkerEvent::Plugin {
                source: PluginSource::Cw,
                event: PluginEvent::Error {
                    message: "x".to_string(),
                    fatal: false,
                },
            },
        ] {
            assert!(!event.is_critical(), "expected bulk event");
        }

        sender.critical(WorkerEvent::FileEnded);
        assert!(matches!(rx.try_recv(), Some(WorkerEvent::FileEnded)));
    }

    #[test]
    fn stopping_ignores_late_connected_but_accepts_shutdown_results() {
        let spec = StreamSpec {
            center_freq_hz: 100_000_000.0,
            sample_rate_hz: 1_000_000.0,
            bandwidth_hz: 1_000_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        assert!(!accept_worker_event(true, &WorkerEvent::Connected(spec)));
        assert!(accept_worker_event(false, &WorkerEvent::Connected(spec)));

        let shutdown_events = [
            info("worker stopping"),
            WorkerEvent::Error("audio failed".to_string()),
            WorkerEvent::RecordingError("recorder failed".to_string()),
            WorkerEvent::RecordingStopped {
                path: PathBuf::from("final.sigmf"),
                samples: 10,
                seconds: 0.1,
            },
            WorkerEvent::Plugin {
                source: PluginSource::Ssdv,
                event: PluginEvent::Error {
                    message: "final image".to_string(),
                    fatal: false,
                },
            },
            WorkerEvent::FileEnded,
        ];
        for event in shutdown_events {
            assert!(
                accept_worker_event(true, &event),
                "shutdown result was ignored"
            );
        }
    }

    #[test]
    fn critical_events_survive_a_full_bulk_queue() {
        let (sender, rx) = EventSender::new();
        let now = Instant::now();
        fill_bulk(&sender, now);
        sender.best_effort_at(info("over capacity"), now);
        assert_eq!(sender.dropped.load(Ordering::Relaxed), 1);

        sender.critical(WorkerEvent::Fatal("panic".to_string()));
        match rx.try_recv() {
            Some(WorkerEvent::Fatal(message)) => assert_eq!(message, "panic"),
            _ => panic!("critical event was dropped or reordered"),
        }

        // The bulk channel still holds the original capacity, and the dropped
        // event is not silently lost: it is reported through the control path.
        let mut bulk_events = 0;
        while let Some(event) = rx.try_recv() {
            assert!(!matches!(event, WorkerEvent::Fatal(_)));
            bulk_events += 1;
        }
        assert_eq!(bulk_events, BULK_EVENT_CAPACITY);

        sender.report_drops_if_due(now + DROP_REPORT_INTERVAL);
        match rx.critical.try_recv() {
            Ok(WorkerEvent::Info(message)) => {
                assert_eq!(message, "1 worker events dropped");
            }
            _ => panic!("drop report was not delivered"),
        }
    }

    #[test]
    fn drop_reports_are_rate_limited() {
        let (sender, rx) = EventSender::new();
        let start = Instant::now();
        fill_bulk(&sender, start);

        sender.best_effort_at(info("drop 1"), start + Duration::from_millis(100));
        sender.best_effort_at(info("drop 2"), start + Duration::from_millis(200));
        assert_eq!(sender.dropped.load(Ordering::Relaxed), 2);

        // Not due yet: no report.
        sender.report_drops_if_due(start + Duration::from_millis(900));
        assert!(rx.critical.try_recv().is_err());

        sender.report_drops_if_due(start + DROP_REPORT_INTERVAL);
        assert!(matches!(rx.critical.try_recv(), Ok(WorkerEvent::Info(_))));
        assert_eq!(sender.dropped.load(Ordering::Relaxed), 0);

        sender.best_effort_at(info("drop 3"), start + Duration::from_millis(1_100));
        assert_eq!(sender.dropped.load(Ordering::Relaxed), 1);
        sender.report_drops_if_due(start + Duration::from_millis(1_500));
        assert!(rx.critical.try_recv().is_err());
        sender.report_drops_if_due(start + Duration::from_secs(2));
        match rx.critical.try_recv() {
            Ok(WorkerEvent::Info(message)) => {
                assert_eq!(message, "1 worker events dropped");
            }
            _ => panic!("second drop report was not delivered"),
        }
    }

    #[test]
    fn critical_events_prefer_the_control_channel() {
        let (sender, rx) = EventSender::new();
        sender.best_effort(info("bulk"));
        sender.critical(WorkerEvent::FileEnded);
        assert!(matches!(rx.try_recv(), Some(WorkerEvent::FileEnded)));
        assert!(matches!(rx.try_recv(), Some(WorkerEvent::Info(_))));
        assert!(rx.try_recv().is_none());
    }
}
