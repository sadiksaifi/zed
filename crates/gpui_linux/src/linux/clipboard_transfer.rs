//! One budget covers target negotiation and every representation of an external clipboard item.
use std::time::{Duration, Instant};

pub(super) const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_secs(4);
pub(super) const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TransferError {
    TimedOut,
    TooLarge,
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TimedOut => "clipboard transfer timed out",
            Self::TooLarge => "clipboard transfer exceeds byte limit",
        })
    }
}

impl std::error::Error for TransferError {}

pub(super) struct ClipboardTransfer {
    deadline: Instant,
    remaining_bytes: usize,
}

impl ClipboardTransfer {
    pub fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            remaining_bytes: MAX_CLIPBOARD_BYTES,
        }
    }

    pub fn remaining_time(&self) -> Result<Duration, TransferError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or(TransferError::TimedOut)
    }

    #[cfg(feature = "x11")]
    pub fn remaining_bytes(&self) -> usize {
        self.remaining_bytes
    }

    pub fn receive(&mut self, length: usize) -> Result<(), TransferError> {
        self.remaining_time()?;
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(length)
            .ok_or(TransferError::TooLarge)?;
        Ok(())
    }
}
