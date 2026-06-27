#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Entry is too large. The limit is 65535.
    #[error("entry too large: {0}")]
    EntryTooLarge(usize),

    /// The sequence number has been purged.
    #[error("seq {0} purged, with oldest seq {1}")]
    SeqPurged(u64, u64),

    /// The sequence number has not been reached.
    #[error("seq {0} not reached, with current seq {1}")]
    SeqNotReached(u64, u64),

    /// No data file in the directory.
    #[error("no data file")]
    NoDataFile,

    /// Invalid data file name.
    #[error("invalid data file name {0}")]
    InvalidDataFilename(String),

    /// This data file may be truncated. More data is expected but EOF is read.
    #[error("data file {0} truncated, with current seq {1}")]
    DataFileTruncated(u64, u64),

    /// Checksum mismatch.
    #[error("data file {0} checksum mismatch, with current seq {1}")]
    ChecksumMismatch(u64, u64),

    /// The size of index file should be multiple of 8.
    #[error("invalid index file {0} size")]
    InvalidIndexFile(u64),

    /// IO error.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Fail to lock the directory.
    #[error(transparent)]
    Lock(#[from] std::fs::TryLockError),
}
