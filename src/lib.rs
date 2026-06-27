#![doc = include_str!("../README.md")]

use crc32fast::hash;
use std::fs::{self, File};
use std::io::{Error, ErrorKind, IoSlice, Read, Result, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const DATA_SUFFIX: &'static str = ".data";
const INDEX_SUFFIX: &'static str = ".index";
const LOCK_FILE: &'static str = "LOCK";

const INDEX_INTERVAL: u64 = 1024;

const READBUF_SIZE: usize = 128 * 1024; // bigger that ENTRY_MAX_LEN

const LEN_SIZE: usize = std::mem::size_of::<u16>();
const CHSUM_SIZE: usize = 2; // use 2 bytes of CRC32
const HEADER_SIZE: usize = LEN_SIZE + CHSUM_SIZE;

const INDEX_SIZE: usize = std::mem::size_of::<u64>();

const ENTRY_MAX_LEN: usize = u16::MAX as usize;

// The state that is shared amount Writer, Syncer, and multiple Readers.
struct SharedState {
    // read-only
    dir: PathBuf,

    // Writer updates this when rotating.
    // Readers use this to scan files.
    file_seqs: RwLock<Vec<Arc<u64>>>,

    // Writer inserts new file's clone into this when rotating.
    // Syncer takes this.
    current_dup: Mutex<Option<File>>,

    // Syncer updates this after syncing.
    sync_seq: AtomicU64,

    // Writer updates this after appending.
    next_seq: AtomicU64,
}

/// SeqLog instance. It is also the Writer.
pub struct SeqLog {
    _lock: File,

    state: Arc<SharedState>,

    // config
    rotate_size: usize,
    rotate_count: usize,

    // Writer's current file status
    current_data: File,
    current_index: File,
    data_file_size: usize,
    file_seq: u64,
}

impl SeqLog {
    /// Create a SeqLog instance with start sequence number, and open it.
    ///
    /// This requires that the parent directory of the given path already
    /// exists and the path itself does not exist.
    ///
    /// This method creates the directory and some required files. If you
    /// want to create it out of your program, use this script:
    ///
    /// ```bash
    /// mkdir $path
    /// touch $path/LOCK
    /// touch $path/`printf "%020d.data" $start_seq`   # first data file
    /// touch $path/`printf "%020d.index" $start_seq`  # first index file
    /// ```
    pub fn create<P: AsRef<Path>>(path: P, start_seq: u64) -> Result<Self> {
        let dir = path.as_ref();

        fs::create_dir(dir)?;

        // LOCK file
        File::create(dir.join(LOCK_FILE))?;

        // first empty data file and index file
        File::create(data_file_name(dir, start_seq))?;
        File::create(index_file_name(dir, start_seq))?;

        Self::open(dir)
    }

    /// Open an existing SeqLog instance.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let dir = path.as_ref();

        let lock = File::open(dir.join(LOCK_FILE))?;
        lock.try_lock()?;

        let file_seqs = list_files(dir)?;

        // at least one data file
        let Some(file_seq) = file_seqs.last() else {
            return error("no data file");
        };

        let file_seq = **file_seq; // deref "&Arc<u64>" to u64

        let fname = data_file_name(dir, file_seq);
        let mut current_data = File::options().append(true).read(true).open(fname)?;

        let fname = index_file_name(dir, file_seq);
        let mut current_index = File::options().append(true).read(true).open(fname)?;

        let (count, file_size) = check_current_file(&mut current_data, &mut current_index)?;

        let next_seq = file_seq + count;
        let data_file_size = file_size as usize;
        dbg!(next_seq);

        let state = SharedState {
            dir: dir.to_path_buf(),
            file_seqs: RwLock::new(file_seqs),
            current_dup: Mutex::new(None),
            sync_seq: AtomicU64::new(next_seq),
            next_seq: AtomicU64::new(next_seq),
        };

        Ok(Self {
            _lock: lock,
            state: Arc::new(state),

            rotate_size: 1024 * 1024 * 1024, // default value: 1G
            rotate_count: 10,                // default value: 10

            current_data,
            current_index,
            data_file_size,
            file_seq,
        })
    }

    /// Append a batch of entries.
    ///
    /// Return `ErrorKind::InvalidData` if any entry is longer than 65535.
    ///
    /// This issues one `write(2)` syscall. Therefore, appending in batches
    /// reduces the number of system calls.
    ///
    /// This writes data into file (page cache), but does not synchronize
    /// disk. Use [`Self::sync`] or [`SeqLogSyncer`] if needed.
    pub fn append<T>(&mut self, entries: &[T]) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        // check rotation
        //
        // We must rotate at the beginning of function, but not at the tail.
        // So uses can always sync() the current file after the function.
        if self.data_file_size >= self.rotate_size {
            self.rotate()?;
        }

        let mut headers = Vec::with_capacity(entries.len());
        let mut bufs = Vec::with_capacity(entries.len() * 2); // header + payload

        let mut total_len = 0;
        let mut next_seq = self.next_seq();

        for entry in entries.iter() {
            let entry = entry.as_ref();
            let len = entry.len();

            if len > ENTRY_MAX_LEN {
                return Err(Error::new(ErrorKind::InvalidData, "too long entry"));
            }

            // encode the header: length + checksum
            let mut header_buf = [0u8; HEADER_SIZE];
            header_buf[..2].copy_from_slice(&(len as u16).to_ne_bytes());
            header_buf[2..].copy_from_slice(&(hash(entry) as u16).to_ne_bytes());
            headers.push(header_buf);

            bufs.push(IoSlice::new(&[])); // hold the place

            // the payload
            bufs.push(IoSlice::new(entry));

            total_len += HEADER_SIZE + len;
            next_seq += 1;

            // append index
            if (next_seq - self.file_seq) % INDEX_INTERVAL == 0 {
                let offset = self.data_file_size + total_len;
                append_index(&mut self.current_index, offset as u64)?;
            }
        }

        // fix headers in buf
        for (i, header) in headers.iter().enumerate() {
            bufs[i * 2] = IoSlice::new(&header[..]);
        }

        // finally, write into file
        let writen_len = self.current_data.write_vectored(&bufs)?;
        if writen_len == 0 {
            return Err(Error::from(ErrorKind::WriteZero));
        }
        if writen_len < total_len {
            // truncate the new data
            self.current_data.set_len(self.data_file_size as u64)?;
            return Err(Error::new(ErrorKind::WriteZero, "write paritial"));
        }

        // update status
        self.data_file_size += total_len;
        self.state.next_seq.store(next_seq, Ordering::Release);

        Ok(())
    }

    /// Remove files containing only entries before the sequence number.
    ///
    /// If a data file contains entries both before and after the sequence
    /// number, it will not be removed.
    ///
    /// If a data file is in read by any reader, it will not be removed.
    ///
    /// The current file is never removed.
    pub fn purge(&mut self, seq: u64) -> Result<()> {
        let mut file_seqs = self.state.file_seqs.write().unwrap();

        let mut until = file_seqs.len() - 1;
        for (i, file_seq) in file_seqs.iter().enumerate() {
            if **file_seq > seq {
                until = i.saturating_sub(1);
                break;
            }
            if Arc::strong_count(file_seq) > 1 {
                until = i;
                break;
            }
        }

        for file_seq in file_seqs.drain(..until) {
            fs::remove_file(data_file_name(&self.state.dir, *file_seq))?;
            fs::remove_file(index_file_name(&self.state.dir, *file_seq))?;
        }
        Ok(())
    }

    /// Truncate entries from the sequence number, inclusive.
    ///
    /// This does not check if any reader are reading these entries.
    pub fn truncate(&mut self, seq: u64) -> Result<()> {
        if seq >= self.next_seq() {
            return Ok(());
        }

        // is the seq in current file?
        if seq < self.file_seq {
            let mut file_seqs = self.state.file_seqs.write().unwrap();

            // we have to keep one file at least, so return error if
            // it would remove all files
            let Some(i) = file_seqs.iter().rposition(|f| **f <= seq) else {
                return Err(Error::new(ErrorKind::NotFound, "seq is expired"));
            };

            // remove some files
            for file_seq in file_seqs.drain(i + 1..) {
                fs::remove_file(data_file_name(&self.state.dir, *file_seq))?;
                fs::remove_file(index_file_name(&self.state.dir, *file_seq))?;
            }

            let file_seq = *file_seqs[i];

            // open new data file and index file
            let fname = data_file_name(&self.state.dir, file_seq);
            self.current_data = File::options().append(true).read(true).open(fname)?;

            let fname = index_file_name(&self.state.dir, file_seq);
            self.current_index = File::options().append(true).read(true).open(fname)?;

            self.data_file_size = 0; // reset later
            self.file_seq = file_seq;

            // update shared state
            self.state.next_seq.store(seq, Ordering::Relaxed);
            self.state.sync_seq.store(seq, Ordering::Relaxed);

            *self.state.current_dup.lock().unwrap() = Some(self.current_data.try_clone()?);
        }

        // remove entries in current data file
        let mut data_buf = Vec::new();
        let (mut file, data_pos) =
            seek_seq_in_file(&self.state.dir, self.file_seq, seq, &mut data_buf)?;
        self.data_file_size = file.stream_position()? as usize - (data_buf.len() - data_pos);
        self.current_data.set_len(self.data_file_size as u64)?;
        self.current_data.seek(SeekFrom::End(0))?;

        // remove indexes in current index file
        let index_file_size = (seq - self.file_seq) / INDEX_INTERVAL * INDEX_SIZE as u64;
        self.current_index.set_len(index_file_size)?;
        self.current_index.seek(SeekFrom::End(0))?;

        Ok(())
    }

    /// Return the next sequence number.
    pub fn next_seq(&self) -> u64 {
        self.state.next_seq()
    }

    /// Return the next sequence number to synchronize to disk.
    ///
    /// This is the synchronization version of [`Self::next_seq`].
    pub fn sync_seq(&self) -> u64 {
        self.state.sync_seq()
    }

    /// Synchronizes entries to disk.
    ///
    /// Syncing data to disk is a relatively slow and blocking operation.
    /// If you do not want to block the current thread, create a
    /// [`SeqLogSyncer`] via [`Self::syncer`] and send it to another thread
    /// to perform synchronization there.
    pub fn sync(&self) -> Result<()> {
        let seq = self.next_seq();

        self.current_data.sync_data()?;

        self.state.sync_seq.store(seq, Ordering::Release);
        Ok(())
    }

    /// Create a [`SeqLogSyncer`].
    pub fn syncer(&self) -> Result<SeqLogSyncer> {
        Ok(SeqLogSyncer {
            state: self.state.clone(),
            current: self.current_data.try_clone()?,
        })
    }

    /// Create a new [`SeqLogReader`] with the sequence number.
    ///
    /// If `synced_only` is set to `true`, the reader will only read entries
    /// that have been synchronized to disk via [`Self::sync`] or [`SeqLogSyncer::sync`].
    pub fn reader(&self, next_seq: u64, synced_only: bool) -> Result<SeqLogReader> {
        let mut data_buf = Vec::new();

        let (current, file_seq, data_pos) = self.state.seek_seq(next_seq, &mut data_buf)?;

        Ok(SeqLogReader {
            state: self.state.clone(),
            data_buf,

            synced_only,

            current,
            file_seq,
            data_pos,
            next_seq,
        })
    }

    /// Configrate rotation.
    ///
    /// `size`: Rotate a data file when it exceeds this size. The default value
    /// is 1G. Setting to 0 means never rotating, and you can call [`Self::rotate`]
    /// to rotate manaully.
    ///
    /// `count`: Number of files to retain. The default value is 10. Setting to
    /// 0 means keeping all files, and you can call [`Self::purge`] to delete
    /// manually.
    pub fn set_rotate(&mut self, size: usize, count: usize) {
        self.rotate_size = if size == 0 { usize::MAX } else { size };
        self.rotate_count = if count == 0 { usize::MAX } else { count };
    }

    /// Rotate the data file manually, e.g. by time or sequence number.
    ///
    /// It does not rotate if the current file is empty.
    ///
    /// You should have called [`Self::set_rotate`] with `size=0` to disable
    /// the automatic rotation.
    pub fn rotate(&mut self) -> Result<()> {
        if self.data_file_size == 0 {
            // do not rotate if current file is empty
            return Ok(());
        }

        let next_seq = self.next_seq();

        // open new file
        self.current_data = File::create_new(data_file_name(&self.state.dir, next_seq))?;
        self.current_index = File::create_new(index_file_name(&self.state.dir, next_seq))?;
        self.data_file_size = 0;
        self.file_seq = next_seq;

        // save new file info
        let mut file_seqs = self.state.file_seqs.write().unwrap();
        file_seqs.push(Arc::new(next_seq));

        // expire
        while file_seqs.len() > self.rotate_count {
            if Arc::strong_count(&file_seqs[0]) > 0 {
                // this file is in used
                break;
            }
            let file_seq = file_seqs.remove(0);
            fs::remove_file(data_file_name(&self.state.dir, *file_seq))?;
            fs::remove_file(index_file_name(&self.state.dir, *file_seq))?;
        }

        // notify the syncer
        *self.state.current_dup.lock().unwrap() = Some(self.current_data.try_clone()?);

        Ok(())
    }
}

/// A sequential reader for scanning entries.
///
/// Compared with key-value stores, SeqLog has two characteristics for reading:
///
/// 1. Seeking to an arbitrary sequence number is not that fast bacause
///    it uses sparse index;
/// 2. Once positioned, sequential reads are very efficient because entries
///    are stored in sequence order.
///
/// This access pattern is similar to a hard disk (HDD).
/// A `SeqLogReader` acts like the disk read head. It is positioned at the
/// specific sequence number when created (by [`SeqLog::reader`]).
/// Subsequent reads (by [`Self::next`]) continue sequentially from that
/// position.
///
/// Each reader maintains its own read position. Multiple readers can
/// coexist and scan the log independently.
///
/// The reader can be sent between threads.
pub struct SeqLogReader {
    state: Arc<SharedState>,

    synced_only: bool,

    current: File,
    file_seq: Arc<u64>,
    data_buf: Vec<u8>,
    data_pos: usize,
    next_seq: u64,
}

impl SeqLogReader {
    /// Return the next entry.
    pub fn next(&mut self) -> Result<Option<&[u8]>> {
        // check if end
        let max_seq = if self.synced_only {
            self.state.sync_seq()
        } else {
            self.state.next_seq()
        };
        if self.next_seq >= max_seq {
            return Ok(None);
        }

        // now there must be an entry

        // if any in the current buffer
        let len = match parse_entry_len(&self.data_buf[self.data_pos..]) {
            Some(len) => len,
            None => {
                // or read more data
                self.read_more_data()?;
                parse_entry_len(&self.data_buf).ok_or(Error::from(ErrorKind::InvalidData))?
            }
        };

        let entry = &self.data_buf[self.data_pos..];
        let payload = &entry[HEADER_SIZE..HEADER_SIZE + len];

        // check checksum
        let chsum1 = parse_checksum(&entry[LEN_SIZE..]);
        let chsum2 = hash(&payload) as u16;
        if chsum1 != chsum2 {
            return error("invalid checksum");
        }

        self.next_seq += 1;
        self.data_pos += HEADER_SIZE + len;
        return Ok(Some(payload));
    }

    // We have checked the seq, so there must be one entry at least.
    // Return Error if can not read more data.
    fn read_more_data(&mut self) -> Result<()> {
        let remain_len = self.data_buf.len() - self.data_pos;
        self.data_buf.copy_within(self.data_pos.., 0);

        self.data_buf.resize(READBUF_SIZE, 0);

        let mut len = self.current.read(&mut self.data_buf[remain_len..])?;

        if len == 0 {
            // end of current file, open new file

            if remain_len != 0 {
                return error("invalid file end");
            }

            let file_seqs = self.state.file_seqs.read().unwrap();
            let Some(file_seq) = file_seqs.iter().rev().find(|&f| **f == self.next_seq) else {
                return error("invalid new file");
            };

            self.file_seq = file_seq.clone();
            self.current = File::open(data_file_name(&self.state.dir, **file_seq))?;

            len = self.current.read(&mut self.data_buf)?;
            if len == 0 {
                return error("invalid file end 2");
            }
        }

        self.data_buf.truncate(remain_len + len);
        self.data_pos = 0;
        Ok(())
    }

    /// Return the next sequence number.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Reset the sequence number. Then [`Self::next`] will return
    /// the entry with this sequence number later.
    ///
    /// This is equvalent to [`SeqLog::reader`] to create a new reader.
    /// This is useful only if you can not access the SeqLog instance.
    pub fn reset(&mut self, seq: u64) -> Result<()> {
        let (file, file_seq, data_pos) = self.state.seek_seq(seq, &mut self.data_buf)?;

        // update
        self.data_pos = data_pos;
        self.file_seq = file_seq;
        self.current = file;
        self.next_seq = seq;
        Ok(())
    }
}

/// A cross-thread synchronizing handler.
///
/// Unlike [`SeqLog::sync`], a `SeqLogSyncer` can be sent to a different thread
/// and used to perform synchronization there. This allows disk sync operations
/// to be offloaded from the writer thread, avoiding stalls caused by slow and
/// blocking storage operations.
pub struct SeqLogSyncer {
    state: Arc<SharedState>,

    current: File,
}

impl SeqLogSyncer {
    /// Synchronize data to disk.
    pub fn sync(&mut self) -> Result<()> {
        // load next_seq before sync()
        let mut next_seq = self.state.next_seq();

        self.current.sync_data()?;

        // check new file rotated
        if let Some(new_file) = self.state.current_dup.lock().unwrap().take() {
            next_seq = self.state.next_seq();
            self.current = new_file;
            self.current.sync_data()?;
        }

        // store sync_seq after sync()
        self.state.sync_seq.store(next_seq, Ordering::Release);
        Ok(())
    }
}

impl SharedState {
    fn seek_seq(&self, seq: u64, data_buf: &mut Vec<u8>) -> Result<(File, Arc<u64>, usize)> {
        if seq > self.next_seq() {
            return Err(Error::new(ErrorKind::NotFound, "seq is too new"));
        }

        // locate the file
        let file_seqs = self.file_seqs.read().unwrap();
        let Some(file_seq) = file_seqs.iter().rev().find(|&f| **f <= seq) else {
            return Err(Error::new(ErrorKind::NotFound, "seq is expired"));
        };

        // lock the file, so the rotate() will not remove this
        let file_seq = file_seq.clone();

        // unlock ASAP
        drop(file_seqs);

        // locate the entry in the file
        let (file, data_pos) = seek_seq_in_file(&self.dir, *file_seq, seq, data_buf)?;

        Ok((file, file_seq, data_pos))
    }

    fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Acquire)
    }

    fn sync_seq(&self) -> u64 {
        self.sync_seq.load(Ordering::Acquire)
    }
}

fn seek_seq_in_file(
    dir: &Path,
    file_seq: u64,
    seq: u64,
    data_buf: &mut Vec<u8>,
) -> Result<(File, usize)> {
    let mut data_file = File::open(data_file_name(dir, file_seq))?;

    // seek by index
    let diff_seq = seq - file_seq;
    if diff_seq >= INDEX_INTERVAL {
        let mut index_file = File::open(index_file_name(dir, file_seq))?;
        index_file.seek(SeekFrom::Start(
            (diff_seq / INDEX_INTERVAL - 1) * INDEX_SIZE as u64,
        ))?;

        let mut index_buf = [0; INDEX_SIZE];
        index_file.read_exact(&mut index_buf)?;

        let offset = parse_index(&index_buf);

        // seek the data file to the offset
        data_file.seek(SeekFrom::Start(offset))?;
    }

    // read data file
    data_buf.resize(READBUF_SIZE, 0);
    let len = data_file.read(data_buf)?;
    if len == 0 {
        return error("seq not found 1");
    }
    data_buf.truncate(len);

    let mut count = diff_seq % INDEX_INTERVAL;
    if count == 0 {
        return Ok((data_file, 0));
    }

    // walk the entries
    loop {
        let mut data_pos = 0;
        while let Some(len) = parse_entry_len(&data_buf[data_pos..]) {
            data_pos += HEADER_SIZE + len;
            count -= 1;
            if count == 0 {
                return Ok((data_file, data_pos));
            }
        }

        if data_buf.len() < READBUF_SIZE {
            return error("seq not found 2");
        }

        data_buf.copy_within(data_pos.., 0);

        let len = data_file.read(&mut data_buf[READBUF_SIZE - data_pos..])?;
        if len == 0 {
            return error("seq not found 3");
        }
        data_buf.truncate(READBUF_SIZE - data_pos + len);
    }
}

// list all files and return the seqs
fn list_files(dir: &Path) -> Result<Vec<Arc<u64>>> {
    let mut files = Vec::new();

    for entry in fs::read_dir(dir)? {
        let path = entry?.path().to_path_buf();
        let fname = path
            .file_name()
            .expect("invalid filename")
            .to_str()
            .expect("fail to parse filename");
        if !fname.ends_with(DATA_SUFFIX) {
            continue;
        }
        let Ok(seq) = fname[..fname.len() - DATA_SUFFIX.len()].parse() else {
            return Err(Error::from(ErrorKind::InvalidFilename));
        };

        files.push(Arc::new(seq));
    }

    files.sort();

    Ok(files)
}

fn check_current_file(data_file: &mut File, index_file: &mut File) -> Result<(u64, u64)> {
    let index_file_len = index_file.metadata()?.len() as usize;
    if index_file_len % INDEX_SIZE != 0 {
        return error("invalid last index file size");
    }

    // read the last index, if any
    let mut count = 0;
    let mut offset = 0;
    if index_file_len != 0 {
        // read the index, get the offset
        let mut index_buf = [0; INDEX_SIZE];
        index_file.seek(SeekFrom::End(-(INDEX_SIZE as i64)))?;
        index_file.read_exact(&mut index_buf)?;
        offset = parse_index(&index_buf);

        // seek the data file to the offset
        data_file.seek(SeekFrom::Start(offset))?;

        count = (index_file_len / INDEX_SIZE) as u64 * INDEX_INTERVAL;
    }

    // read data
    let mut data_buf = Vec::new();
    data_file.read_to_end(&mut data_buf)?;

    // walk entries to the end
    let mut entry = &data_buf[..];
    while let Some(len) = parse_entry_len(entry) {
        entry = &entry[HEADER_SIZE + len..];

        count += 1;

        offset += (HEADER_SIZE + len) as u64;
        if count % INDEX_INTERVAL == 0 {
            append_index(index_file, offset)?;
        }
    }

    Ok((count, offset))
}

// append index to index file
fn append_index(file: &mut File, offset: u64) -> Result<()> {
    file.write_all(&offset.to_ne_bytes())
}

// build data file name
fn data_file_name(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{:020}{}", seq, DATA_SUFFIX))
}

// build index file name
fn index_file_name(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{:020}{}", seq, INDEX_SUFFIX))
}

// return None if not complete
fn parse_entry_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < HEADER_SIZE {
        return None;
    }
    let len = u16::from_ne_bytes(buf[0..LEN_SIZE].try_into().unwrap()) as usize;
    if buf.len() < HEADER_SIZE + len {
        return None;
    }
    Some(len)
}

fn parse_checksum(buf: &[u8]) -> u16 {
    u16::from_ne_bytes(buf[0..CHSUM_SIZE].try_into().unwrap())
}

fn parse_index(buf: &[u8]) -> u64 {
    u64::from_ne_bytes(buf[..8].try_into().unwrap())
}

fn error<T>(detail: impl Into<String>) -> Result<T> {
    Err(Error::new(ErrorKind::InvalidData, detail.into()))
}
