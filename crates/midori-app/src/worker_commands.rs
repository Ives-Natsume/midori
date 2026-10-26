//! Worker command channel with reliable delivery.
//!
//! The UI is the only producer and commands are generated at human rates, so a
//! bounded channel is enough as long as a send waits briefly instead of being
//! dropped with `try_send`. A timeout keeps a wedged worker from freezing the
//! UI forever; the caller turns the error into a log entry.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use midori_core::DemodMode;

pub(crate) const COMMAND_CAPACITY: usize = 16;
pub(crate) const COMMAND_SEND_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy)]
pub(crate) struct DemodUpdate {
    pub(crate) mode: DemodMode,
    pub(crate) bandwidth_hz: f64,
    pub(crate) audio_bandwidth_hz: f64,
    pub(crate) deemphasis_us: f64,
    pub(crate) bfo_hz: f64,
    pub(crate) channel_rate_hz: f64,
}

pub(crate) enum WorkerCommand {
    /// Apply a VFO offset and, optionally, retune the hardware in one step so
    /// a retune can never be half-applied (offset cleared but frequency kept,
    /// or the reverse).
    SetTuning {
        frequency_hz: Option<f64>,
        vfo_offset_hz: f64,
    },
    SetGain(f64),
    SetDemod(DemodUpdate),
    SetCwAfc(bool),
    AutoTune,
    StartRecording(PathBuf),
    StopRecording,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandSendError {
    Timeout(Duration),
    Disconnected,
}

impl fmt::Display for CommandSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(timeout) => write!(
                formatter,
                "worker did not accept the command within {} ms",
                timeout.as_millis()
            ),
            Self::Disconnected => formatter.write_str("worker is not running"),
        }
    }
}

pub(crate) struct CommandSender {
    tx: Sender<WorkerCommand>,
}

impl CommandSender {
    pub(crate) fn new() -> (Self, Receiver<WorkerCommand>) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        (Self { tx }, rx)
    }

    /// Send a command, waiting up to [`COMMAND_SEND_TIMEOUT`] for capacity.
    pub(crate) fn send(&self, command: WorkerCommand) -> Result<(), CommandSendError> {
        self.send_with_timeout(command, COMMAND_SEND_TIMEOUT)
    }

    fn send_with_timeout(
        &self,
        command: WorkerCommand,
        timeout: Duration,
    ) -> Result<(), CommandSendError> {
        match self.tx.send_timeout(command, timeout) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Timeout(_)) => Err(CommandSendError::Timeout(timeout)),
            Err(SendTimeoutError::Disconnected(_)) => Err(CommandSendError::Disconnected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_commands_in_order() {
        let (sender, receiver) = CommandSender::new();
        sender.send(WorkerCommand::SetGain(12.0)).unwrap();
        sender.send(WorkerCommand::StopRecording).unwrap();

        match receiver.try_recv() {
            Ok(WorkerCommand::SetGain(db)) => assert_eq!(db, 12.0),
            _ => panic!("first command missing"),
        }
        assert!(matches!(
            receiver.try_recv(),
            Ok(WorkerCommand::StopRecording)
        ));
    }

    #[test]
    fn reports_timeout_instead_of_dropping_when_full() {
        let (sender, receiver) = CommandSender::new();
        for _ in 0..COMMAND_CAPACITY {
            sender.send(WorkerCommand::SetCwAfc(true)).unwrap();
        }

        let timeout = Duration::from_millis(10);
        let error = sender
            .send_with_timeout(WorkerCommand::Stop, timeout)
            .unwrap_err();
        assert_eq!(error, CommandSendError::Timeout(timeout));

        // Once the worker drains one slot, the reliable send goes through.
        assert!(receiver.try_recv().is_ok());
        sender
            .send_with_timeout(WorkerCommand::Stop, timeout)
            .unwrap();
        let mut saw_stop = false;
        while let Ok(command) = receiver.try_recv() {
            saw_stop |= matches!(command, WorkerCommand::Stop);
        }
        assert!(saw_stop);
    }

    #[test]
    fn reports_a_disconnected_worker() {
        let (sender, receiver) = CommandSender::new();
        drop(receiver);
        assert_eq!(
            sender.send(WorkerCommand::Stop).unwrap_err(),
            CommandSendError::Disconnected
        );
    }
}
