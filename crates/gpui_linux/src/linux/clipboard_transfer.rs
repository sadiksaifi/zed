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

impl From<TransferError> for gpui::ClipboardReadError {
    fn from(error: TransferError) -> Self {
        match error {
            TransferError::TimedOut => Self::TimedOut,
            TransferError::TooLarge => Self::TooLarge,
        }
    }
}

/// A clipboard read prepared on the main thread. A selection that GPUI owns answers at once; an
/// external transfer runs on the background executor so a slow owner cannot block the main
/// thread. The transfer's deadline starts when the read is prepared.
pub(crate) enum PreparedRead<T> {
    Ready(T),
    Transfer(Box<dyn FnOnce() -> T + Send>),
}

impl<T: Send + 'static> PreparedRead<T> {
    pub fn transfer(transfer: impl FnOnce() -> T + Send + 'static) -> Self {
        Self::Transfer(Box::new(transfer))
    }

    /// Dropping the task discards the result; an external transfer still ends by its deadline.
    pub fn spawn(self, executor: &gpui::BackgroundExecutor) -> gpui::Task<T> {
        match self {
            Self::Ready(value) => gpui::Task::ready(value),
            Self::Transfer(transfer) => executor.spawn(async move { transfer() }),
        }
    }
}

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

    /// Narrows the payload budget after bounded representation negotiation.
    pub fn limit_bytes(&mut self, max_bytes: usize) {
        self.remaining_bytes = self.remaining_bytes.min(max_bytes);
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
