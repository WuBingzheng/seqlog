#[derive(Debug, thiserror::Error)]
pub enum Error {
    // invalid input
    #[error("entry too large: {0}")]
    EntryTooLarge(usize),

    #[error("seq {0} purged, with oldest seq {1}")]
    SeqPurged(u64, u64),

    #[error("seq {0} not reached, with current seq {1}")]
    SeqNotReached(u64, u64),

    // corrupted store
    #[error("no data file")]
    NoDataFile,

    #[error("invalid data file name {0}")]
    InvalidDataFilename(String),

    #[error("data file {0} not found")]
    DataFileNotFound(u64),

    #[error("data file {0} truncated, with current seq {1}")]
    DataFileTruncated(u64, u64),

    #[error("data file {0} checksum mismatch, with current seq {1}")]
    ChecksumMismatch(u64, u64),

    #[error("invalid index file {0} size")]
    InvalidIndexFile(u64),

    // IO
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Lock(#[from] std::fs::TryLockError),
}
