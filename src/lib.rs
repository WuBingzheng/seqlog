#![doc = include_str!("../README.md")]

use crc32fast::hash;
use std::fs::{self, File};
use std::io::{Error, ErrorKind, IoSlice, Read, Result, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const DATA_SUFFIX: &'static str = ".data";
const INDEX_SUFFIX: &'static str = ".index";
const LOCK_FILE: &'static str = "LOCK";

const INDEX_INTERVAL: u64 = 1024;

const READBUF_SIZE: usize = 128 * 1024; // bigger that ENTRY_MAX_LEN

const LEN_SIZE: usize = std::mem::size_of::<u16>();
const CHSUM_SIZE: usize = 2;
const HEADER_SIZE: usize = LEN_SIZE + CHSUM_SIZE;
const INDEX_SIZE: usize = std::mem::size_of::<u64>();

const ENTRY_MAX_LEN: usize = u16::MAX as usize;

struct DataFile {
    // the file name, also the first entry seq in this file
    seq: u64,

    // by Readers
    refers: AtomicUsize,
}

// The state that is shared amount Writer, Syncer, and multiple Readers.
struct SharedState {
    dir: PathBuf,

    // Writer updates this when rotating.
    // Readers use this to scan files.
    data_files: RwLock<Vec<DataFile>>,

    // Writer inserts new file's clone into this when rotating.
    // Syncer takes this.
    current_dup: Mutex<Option<File>>,

    // Syncer updates this after syncing.
    synced_seq: AtomicU64,

    // Writer updates this after appending.
    next_seq: AtomicU64,
}

/// SeqLog instance.
///
/// This is also the Writer.
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
}

impl SeqLog {
    /// Create a SeqLog instance with start sequence number, and open it.
    ///
    /// This mainly creates a directory using [`std::fs::create_dir`],
    /// which requires that the parent directory of the given path already
    /// exists and the path itself does not exist. It's equvalent to `mkdir`
    /// without `-p`.
    pub fn create<P: AsRef<Path>>(path: P, start_seq: u64) -> Result<Self> {
        let dir = path.as_ref();

        fs::create_dir(dir)?;

        // LOCK file holds the start_seq, for nothing
        fs::write(dir.join(LOCK_FILE), start_seq.to_string())?;

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

        let data_files = list_data_files(dir)?;

        // at least one data file
        let Some(current_data_file) = data_files.last() else {
            return Err(Error::new(ErrorKind::InvalidData, "no data file"));
        };

        let fname = data_file_name(dir, current_data_file.seq);
        let mut current_data = File::options().append(true).read(true).open(fname)?;

        let fname = index_file_name(dir, current_data_file.seq);
        let mut current_index = File::options().append(true).read(true).open(fname)?;

        let (count, file_size) = check_current_file(&mut current_data, &mut current_index)?;

        let next_seq = current_data_file.seq + count;
        dbg!(current_data_file.seq);
        let data_file_size = file_size as usize;
        dbg!(next_seq);

        let state = SharedState {
            dir: dir.to_path_buf(),
            data_files: RwLock::new(data_files),
            current_dup: Mutex::new(None),
            synced_seq: AtomicU64::new(next_seq),
            next_seq: AtomicU64::new(next_seq),
        };

        Ok(Self {
            _lock: lock,
            state: Arc::new(state),

            rotate_size: 1024 * 1024 * 1024, // default value: 1G
            rotate_count: 20,                // default value: 20

            current_data,
            current_index,
            data_file_size,
        })
    }

    /// Append a batch of entries.
    ///
    /// Return `ErrorKind::InvalidData` if any entry is empty (length=0) or
    /// longer than 65526 (64K - 10).
    ///
    /// This issues one `write(2)` syscall. Therefore, appending in batches
    /// reduces the number of system calls.
    ///
    /// This writes data into file (page cache), but does not synchronize
    /// disk. You should call [`Self::sync`] if needed.
    pub fn append<T>(&mut self, entries: &[T]) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        // check rotation
        //
        // We can not check and rotate at the tail of this function, because
        // that may close the current file and user can not call sync() on it.
        if self.data_file_size >= self.rotate_size {
            self.rotate()?;
        }

        let mut lengths = Vec::with_capacity(entries.len());
        let mut chsums = Vec::with_capacity(entries.len());
        let mut bufs = Vec::with_capacity(entries.len() * 2);

        let mut total_len = 0;
        let mut next_seq = self.next_seq();

        for entry in entries.iter() {
            let entry = entry.as_ref();
            let len = entry.len();

            if len > ENTRY_MAX_LEN {
                return Err(Error::new(ErrorKind::InvalidData, "too long entry"));
            }

            // encode the length
            lengths.push((len as u16).to_ne_bytes());
            chsums.push((hash(entry) as u16).to_ne_bytes());
            bufs.push(IoSlice::new(&[])); // hold the place
            bufs.push(IoSlice::new(&[])); // hold the place

            // encode the entry
            bufs.push(IoSlice::new(entry));

            total_len += HEADER_SIZE + len;

            next_seq += 1;
            if next_seq % INDEX_INTERVAL == 0 {
                let offset = self.data_file_size + total_len;
                append_index(&mut self.current_index, offset as u64)?;
            }
        }

        // fix headers in buf
        for (i, (len_buf, chs_buf)) in lengths.iter().zip(chsums.iter()).enumerate() {
            bufs[i * 3] = IoSlice::new(len_buf);
            bufs[i * 3 + 1] = IoSlice::new(chs_buf);
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

    /// Remove files that contain only entries before the sequence number.
    ///
    /// If a data file contains entries both before and after the sequence
    /// number, it will not be removed.
    ///
    /// If a data file is in read by any reader, it will not be removed.
    pub fn purge(&mut self, before_seq: u64) -> Result<()> {
        let mut data_files = self.state.data_files.write().unwrap();

        let mut until = data_files.len() - 1;
        for (i, data_file) in data_files.iter().enumerate() {
            if data_file.seq > before_seq {
                until = i.saturating_sub(1);
                break;
            }
            if data_file.refers.load(Ordering::Relaxed) != 0 {
                until = i;
                break;
            }
        }

        for data_file in data_files.drain(..until) {
            fs::remove_file(data_file_name(&self.state.dir, data_file.seq))?;
            fs::remove_file(index_file_name(&self.state.dir, data_file.seq))?;
        }
        Ok(())
    }

    /// Truncate entries exactly after the sequence number.
    ///
    /// This does not check if any reader are reading these entries. TODO
    pub fn truncate(&mut self, _after_seq: u64) -> Result<()> {
        // if after_seq >= self.next_seq() {
        //     return Ok(());
        // }

        // let current_start_seq = read_block_seq(&mut self.current, 0)?;

        // // after_seq is in current file
        // if current_start_seq <= after_seq {
        //     // locate block in file
        //     let (block_index, block_seq) =
        //         locate_block(&mut self.current, current_start_seq, after_seq)?;

        //     // locate entry in block
        //     let mut block_buf = Vec::new();
        //     let block_pos = locate_entry(
        //         &mut self.current,
        //         &mut block_buf,
        //         block_index,
        //         block_seq,
        //         after_seq,
        //     )?;

        //     self.file_size = block_index * BLOCK_SIZE + block_pos;
        //     self.current.set_len(self.file_size as u64)?;

        //     self.state.next_seq.store(after_seq, Ordering::Relaxed);
        //     self.state.synced_seq.store(after_seq, Ordering::Relaxed);

        // // after_seq is in older files
        // } else {
        //     todo!();
        // }

        Ok(())
    }

    /// Return the next sequence number.
    pub fn next_seq(&self) -> u64 {
        self.state.next_seq.load(Ordering::Relaxed)
    }

    pub fn synced_seq(&self) -> u64 {
        self.state.synced_seq.load(Ordering::Relaxed)
    }

    /// Synchronize new data onto disk.
    pub fn sync(&self) -> Result<()> {
        self.current_data.sync_data()?;
        self.state
            .synced_seq
            .store(self.next_seq(), Ordering::Relaxed);
        Ok(())
    }

    /// Create a Syncer.
    pub fn syncer(&self) -> Result<SeqLogSyncer> {
        Ok(SeqLogSyncer {
            state: self.state.clone(),
            current: self.current_data.try_clone()?,
        })
    }

    /// Create a new [`SeqLogReader`] with the sequence number.
    pub fn reader(&self, next_seq: u64) -> Result<SeqLogReader> {
        let mut data_buf = Vec::new();

        let (current, file_seq, data_pos) = seek_seq(
            &self.state.dir,
            &self.state.data_files,
            next_seq,
            &mut data_buf,
        )?;

        Ok(SeqLogReader {
            state: self.state.clone(),
            data_buf,

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
    /// `count`: Number of files to retain. The default value is 20. Setting to
    /// 0 means keeping all files, and you can call [`Self::purge`] to delete
    /// manually.
    pub fn set_rotate(&mut self, size: usize, count: usize) {
        self.rotate_size = if size == 0 { usize::MAX } else { size };
        self.rotate_count = if count == 0 { usize::MAX } else { count };
    }

    /// Rotate the data file manually, e.g. by time or sequence number.
    ///
    /// Return the new file name, so you can handle it, such as make a symlink.
    ///
    /// It does not rotate if the current file is empty.
    ///
    /// You should have called [`Self::set_rotate`] with `size=0` to disable
    /// the automatic rotation.
    pub fn rotate(&mut self) -> Result<PathBuf> {
        let next_seq = self.next_seq();

        let data_fname = data_file_name(&self.state.dir, next_seq);
        if self.data_file_size == 0 {
            // do not rotate if current file is empty
            return Ok(data_fname);
        }

        // open new file
        self.current_data = File::create_new(&data_fname)?;
        self.current_index = File::create_new(index_file_name(&self.state.dir, next_seq))?;

        self.data_file_size = 0;

        // save new file info
        let new_file = DataFile {
            seq: next_seq,
            refers: AtomicUsize::new(0),
        };

        let mut data_files = self.state.data_files.write().unwrap();
        data_files.push(new_file);

        // expire
        while data_files.len() > self.rotate_count {
            if data_files[0].refers.load(Ordering::Relaxed) > 0 {
                // this file is in used
                break;
            }
            let data_file = data_files.remove(0);
            fs::remove_file(data_file_name(&self.state.dir, data_file.seq))?;
            fs::remove_file(index_file_name(&self.state.dir, data_file.seq))?;
        }

        // tell syncer
        *self.state.current_dup.lock().unwrap() = Some(self.current_data.try_clone()?);

        Ok(data_fname)
    }

    /// Remove all data and start at the new sequence number.
    ///
    /// This renames the directory to `backup_dir`, creates a new SeqLog instance,
    /// and replaces `self` with the new instance.
    ///
    /// All entries in SeqLog must have contiguous sequence numbers. When you
    /// need to skip some sequence numbers (e.g., after loading a new snapshot),
    /// the only way is to clean up the old data and create a new SeqLog
    /// instance starting from the new sequence number using this method.
    pub fn reset<P: AsRef<Path>>(&mut self, next_seq: u64, backup_dir: P) -> Result<()> {
        fs::rename(&self.state.dir, backup_dir)?;

        *self = Self::create(&self.state.dir, next_seq)?;

        Ok(())
    }
}

fn read_index(buf: &[u8]) -> u64 {
    u64::from_ne_bytes(buf[..8].try_into().unwrap())
}

/// A sequential reader for scanning entries in a SeqLog.
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

    current: File,
    file_seq: u64,
    data_buf: Vec<u8>,
    data_pos: usize,
    next_seq: u64,
}

impl SeqLogReader {
    /// Return the next entry.
    pub fn next(&mut self) -> Result<Option<&[u8]>> {
        dbg!(self.next_seq);
        // check any entry in current data buffer
        if let Some(len) = parse_entry_len(&self.data_buf[self.data_pos..]) {
            // yes, happy path
            self.next_seq += 1;
            self.data_pos += HEADER_SIZE + len;
            return Ok(Some(&self.data_buf[self.data_pos - len..self.data_pos]));
        }

        // read more from data file
        let remain_len = self.data_buf.len() - self.data_pos;
        self.data_buf.copy_within(self.data_pos.., 0);

        let len = self.current.read(&mut self.data_buf[remain_len..])?;
        if len == 0 {
            todo!("EOF");
        }
        self.data_buf.truncate(remain_len + len);
        self.data_pos = 0;

        self.next() // TODO

        /*
        // read more data

        // If the current block is full-size, read the next block into
        // block_buf (buf_base=0).
        // If non-full-size block, this block was the end of the file
        // when read, but by now, there maybe new data appended, so we
        // try to read more data into the bottom of the block_buf.
        let mut buf_base = 0;
        if self.block_buf.len() != BLOCK_SIZE {
            buf_base = self.block_buf.len();
            self.block_buf.resize(BLOCK_SIZE, 0);
        }

        let mut read_len = self.current.read(&mut self.block_buf[buf_base..])?;

        // if read nothing, try to open new file
        if read_len == 0 {
            // search the next file
            // TODO optimize this, this is very often
            let data_files = self.state.data_files.read().unwrap();
            let Ok(i) = data_files.binary_search_by(|f| f.seq.cmp(&self.next_seq)) else {
                // no more file, roll back the block_buf and return EOF
                if buf_base != 0 {
                    self.block_buf.truncate(buf_base);
                }
                return Ok(None);
            };

            // TODO if same file, do not open

            // open new file
            self.current = File::open(file_name(&self.state.dir, data_files[i].seq))?;

            // read the first block
            read_len = self.current.read(&mut self.block_buf)?;

            // reset because of new block
            buf_base = 0;
        }

        self.block_buf.truncate(buf_base + read_len);

        // if new block
        if buf_base == 0 {
            // check new block's start-seq
            let start_seq = parse_seq(&self.block_buf);
            if start_seq != self.next_seq {
                return Err(Error::new(ErrorKind::InvalidData, "non-consecutive seq"));
            }

            // reset
            self.block_pos = SEQ_SIZE;
        }

        // try recursively
        self.next()
        */
    }

    /// Return the next sequence number.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Reset the sequence number. Then [`Self::next`] will return
    /// the entry with this sequence number later.
    ///
    /// This also resets the entire SeqLogReader instance.
    ///
    /// This is expensive and you should not call this often.
    pub fn reset(&mut self, seq: u64) -> Result<()> {
        let (file, file_seq, data_pos) = seek_seq(
            &self.state.dir,
            &self.state.data_files,
            seq,
            &mut self.data_buf,
        )?;

        // unlock original file
        let data_files = self.state.data_files.read().unwrap();
        let i = data_files
            .binary_search_by_key(&self.file_seq, |f| f.seq)
            .unwrap();
        data_files[i].refers.fetch_sub(1, Ordering::Relaxed); // unlock the file

        // update
        self.data_pos = data_pos;
        self.file_seq = file_seq;
        self.current = file;
        self.next_seq = seq;
        Ok(())
    }
}

pub struct SeqLogSyncer {
    state: Arc<SharedState>,

    current: File,
}

impl SeqLogSyncer {
    pub fn sync(&mut self) -> Result<()> {
        // load next_seq before sync()
        let mut next_seq = self.state.next_seq.load(Ordering::Acquire);

        self.current.sync_data()?;

        if let Some(new_file) = self.state.current_dup.lock().unwrap().take() {
            next_seq = self.state.next_seq.load(Ordering::Acquire);
            self.current = new_file;
            self.current.sync_data()?;
        }

        // store synced_seq after sync()
        self.state.synced_seq.store(next_seq, Ordering::Release);
        Ok(())
    }
}

// seek the sequence number in all data files
fn seek_seq(
    dir: &Path,
    arc_data_files: &RwLock<Vec<DataFile>>,
    seq: u64,
    data_buf: &mut Vec<u8>,
) -> Result<(File, u64, usize)> {
    // if seq > self.main.next_seq {
    //     return Ok(false);
    // }

    // locate the file
    let data_files = arc_data_files.read().unwrap();
    let Some(data_file) = data_files.iter().rev().find(|&f| f.seq <= seq) else {
        return Err(Error::new(ErrorKind::NotFound, "seq is expired"));
    };

    let (file, data_pos) = seek_seq_in_file(dir, data_file.seq, seq, data_buf)?;

    data_file.refers.fetch_add(1, Ordering::Relaxed); // lock the file

    dbg!(data_pos);
    Ok((file, data_file.seq, data_pos))
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
    dbg!(file_seq, seq, diff_seq);
    if diff_seq >= INDEX_INTERVAL {
        let mut index_file = File::open(index_file_name(dir, file_seq))?;
        index_file.seek(SeekFrom::Start(
            (diff_seq / INDEX_INTERVAL - 1) * INDEX_SIZE as u64,
        ))?;

        let mut index_buf = [0; INDEX_SIZE];
        index_file.read_exact(&mut index_buf)?;

        let offset = read_index(&index_buf);
        dbg!(offset);

        // seek the data file to the offset
        data_file.seek(SeekFrom::Start(offset))?;
    }

    // read data file
    data_buf.resize(READBUF_SIZE, 0);
    let len = data_file.read(data_buf)?;
    dbg!(len);
    if len == 0 {
        return error("seq not found 1");
    }
    data_buf.truncate(len);

    let mut count = diff_seq % INDEX_INTERVAL;
    if count == 0 {
        return Ok((data_file, 0));
    }

    dbg!(count);

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
            dbg!(file_seq, data_buf.len(), data_pos, count);
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

fn list_data_files(dir: &Path) -> Result<Vec<DataFile>> {
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

        files.push(DataFile {
            seq,
            refers: AtomicUsize::new(0),
        });
    }

    // sort by start_seq
    files.sort_by_key(|f| f.seq);

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
        offset = read_index(&index_buf);

        // seek the data file to the offset
        data_file.seek(SeekFrom::Start(offset))?;

        count = (index_file_len / INDEX_SIZE) as u64 * INDEX_INTERVAL;
    }
    dbg!(count);

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
    dbg!(count);

    Ok((count, offset))
}

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

fn error<T>(detail: impl Into<String>) -> Result<T> {
    Err(Error::new(ErrorKind::InvalidData, detail.into()))
}
