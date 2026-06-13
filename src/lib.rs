#![doc = include_str!("../README.md")]

use std::fs::{self, File};
use std::io::{Error, ErrorKind, IoSlice, Read, Result, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

const LOCK_FILE: &'static str = "LOCK";
const SUBFIX: &'static str = ".seqlog";

const SEQ_SIZE: usize = std::mem::size_of::<u64>();
const LEN_SIZE: usize = std::mem::size_of::<u16>();

const BLOCK_SIZE: usize = 64 * 1024;
const ENTRY_MAX_LEN: usize = BLOCK_SIZE - SEQ_SIZE - LEN_SIZE;

struct DataFile {
    file_no: u64,
    start_seq: u64,
    path: PathBuf,
    refers: AtomicUsize,
}

/// SeqLog instance.
pub struct SeqLog {
    path: PathBuf,

    // config
    rotate_size: usize,
    rotate_count: usize,

    // files
    _lock: File,
    data_files: Arc<RwLock<Vec<DataFile>>>,

    // current file status
    current: File,
    file_size: usize,
    block_left: usize,

    next_seq: u64,
}

impl SeqLog {
    /// Create a SeqLog instance with start sequence number, and open it.
    ///
    /// This mainly creates a directory using [`std::fs::create_dir`],
    /// which requires that the parent directory of the given path already
    /// exists and the path itself does not exist. It's equvalent to `mkdir`
    /// without `-p`.
    pub fn create<P: AsRef<Path>>(path: P, start_seq: u64) -> Result<Self> {
        let path = path.as_ref();

        fs::create_dir(path)?;

        // LOCK file holds the start_seq, for nothing
        fs::write(path.join(LOCK_FILE), start_seq.to_string())?;

        // first data file
        fs::write(path.join(file_name(1)), start_seq.to_ne_bytes())?;

        Self::open(path)
    }

    /// Open an existing SeqLog instance.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        let lock = File::open(path.join(LOCK_FILE))?;
        lock.try_lock()?;

        let data_files = load_data_files(path)?;

        let current_info = data_files.last().unwrap();

        let (next_seq, file_size) = read_file_info(&current_info.path)?;
        let block_left = BLOCK_SIZE - (file_size % BLOCK_SIZE);
        dbg!(next_seq);

        let current = File::options()
            .read(true)
            .append(true)
            .open(&current_info.path)?;

        Ok(Self {
            path: path.to_path_buf(),
            rotate_size: 1024 * 1024 * 1024, // 1G
            rotate_count: 20,
            _lock: lock,
            data_files: Arc::new(RwLock::new(data_files)),
            current,
            file_size,
            block_left,
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
        if self.file_size >= self.rotate_size {
            self.rotate()?;
        }

        let mut blocks = Vec::new(); // block header: start seq
        let mut lengths = Vec::with_capacity(entries.len()); // lengths for every entries
        let mut bufs = Vec::with_capacity(entries.len() + 2);

        let mut block_left = self.block_left;

        const ZEROS: [u8; LEN_SIZE] = [0; _];
        let dummy = IoSlice::new(&ZEROS);

        // build @bufs by @entries
        for (i, entry) in entries.iter().enumerate() {
            let entry = entry.as_ref();
            let len = entry.len();

            if len == 0 {
                return Err(Error::new(ErrorKind::InvalidData, "empty entry"));
            }
            if len > ENTRY_MAX_LEN {
                return Err(Error::new(ErrorKind::InvalidData, "too long entry"));
            }

            if LEN_SIZE + len > block_left {
                // we need a new block

                // padding the block
                if block_left <= LEN_SIZE {
                    bufs.push(IoSlice::new(&ZEROS[..block_left]));
                } else {
                    // push 2 zeros to indicate the end of block
                    bufs.push(IoSlice::new(&ZEROS[..LEN_SIZE]));
                    // pad the remaining by this entry which is long enough
                    bufs.push(IoSlice::new(&entry[..block_left - LEN_SIZE]));
                }

                // new block
                blocks.push((bufs.len(), (self.next_seq + i as u64).to_ne_bytes()));
                bufs.push(dummy); // hold the place for block header

                block_left = BLOCK_SIZE - SEQ_SIZE;
            }

            // encode the length
            lengths.push((bufs.len(), (len as u16).to_ne_bytes()));
            bufs.push(dummy); // hold the place for length

            // encode the entry
            bufs.push(IoSlice::new(entry));

            block_left -= LEN_SIZE + len;
        }

        // fix the bufs for segments header: lengths
        for (i, len) in lengths.iter() {
            bufs[*i] = IoSlice::new(len);
        }

        // fix the bufs for blocks header: start_seg
        for (i, seq) in blocks.iter() {
            bufs[*i] = IoSlice::new(seq);
        }

        let total_len = blocks.len() * BLOCK_SIZE + self.block_left - block_left;

        // finally, write into file
        let writen_len = self.current.write_vectored(&bufs)?;
        if writen_len == 0 {
            return Err(Error::from(ErrorKind::WriteZero));
        }
        if writen_len < total_len {
            // truncate the new data
            self.current.set_len((self.file_size - writen_len) as u64)?;
            return Err(Error::new(ErrorKind::WriteZero, "write paritial"));
        }

        // update status
        self.block_left = block_left;
        self.next_seq += entries.len() as u64;
        self.file_size += total_len;

        Ok(())
    }

    /// Synchronize new data onto disk.
    pub fn sync(&self) -> Result<()> {
        self.current.sync_data()
    }

    /// Remove files that contain only entries before the sequence number.
    ///
    /// If a data file contains entries both before and after the sequence
    /// number, it will not be removed.
    ///
    /// If a data file is in read by any reader, it will not be removed.
    pub fn purge(&mut self, before_seq: u64) -> Result<()> {
        let mut data_files = self.data_files.write().unwrap();

        let mut until = data_files.len() - 1;
        for (i, data_file) in data_files.iter().enumerate() {
            if data_file.start_seq > before_seq {
                until = i.saturating_sub(1);
                break;
            }
            if data_file.refers.load(Ordering::Relaxed) != 0 {
                until = i;
                break;
            }
        }

        for data_file in data_files.drain(..until) {
            fs::remove_file(data_file.path)?;
        }
        Ok(())
    }

    /// Truncate entries exactly after the sequence number.
    ///
    /// This does not check if any reader are reading these entries. TODO
    pub fn truncate(&mut self, after_seq: u64) -> Result<()> {
        if after_seq >= self.next_seq {
            return Ok(());
        }

        let current_start_seq = read_block_seq(&mut self.current, 0)?;

        // after_seq is in current file
        if current_start_seq <= after_seq {
            // locate block in file
            let (block_index, block_seq) =
                locate_block(&mut self.current, current_start_seq, after_seq)?;

            // locate entry in block
            let mut block_buf = Vec::new();
            let block_pos = locate_entry(
                &mut self.current,
                &mut block_buf,
                block_index,
                block_seq,
                after_seq,
            )?;

            self.file_size = block_index * BLOCK_SIZE + block_pos;
            self.current.set_len(self.file_size as u64)?;
            self.block_left = BLOCK_SIZE - block_pos;
            self.next_seq = after_seq;

        // after_seq is in older files
        } else {
            todo!();
        }

        Ok(())
    }

    /// Return the next sequence number.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Create a new [`SeqLogReader`] with the start sequence number.
    pub fn reader(&self, start_seq: u64) -> Result<SeqLogReader> {
        let mut block_buf = Vec::new();
        let (file, file_no, block_pos) = seek_seq(&self.data_files, start_seq, &mut block_buf)?;

        Ok(SeqLogReader {
            data_files: self.data_files.clone(),

            current: file,
            file_no,
            block_buf,
            block_pos,
            next_seq: start_seq,
        })
    }

    /// Rotate the data file manually, e.g. by time or sequence number.
    ///
    /// You should have called [`Self::set_rotate`] with `size=0` to disable
    /// the automatic rotation.
    pub fn rotate(&mut self) -> Result<PathBuf> {
        // open new file
        let new_file_no = self.data_files.read().unwrap().last().unwrap().file_no + 1;
        let path = self.path.join(file_name(new_file_no));

        self.current = File::create_new(&path)?;
        self.current.write(&self.next_seq.to_ne_bytes())?;

        self.file_size = SEQ_SIZE;
        self.block_left = BLOCK_SIZE - SEQ_SIZE;

        // save new file info
        let new_file = DataFile {
            file_no: new_file_no,
            start_seq: self.next_seq,
            path: path.clone(),
            refers: AtomicUsize::new(0),
        };

        let mut data_files = self.data_files.write().unwrap();
        data_files.push(new_file);

        // expire
        while data_files.len() > self.rotate_count {
            if data_files[0].refers.load(Ordering::Relaxed) > 0 {
                // this file is in used
                break;
            }
            let data_file = data_files.remove(0);
            fs::remove_file(data_file.path)?;
        }

        Ok(path)
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
    pub fn reset<P: AsRef<Path>>(&mut self, start_seq: u64, backup_dir: P) -> Result<()> {
        fs::rename(&self.path, backup_dir)?;

        *self = Self::create(&self.path, start_seq)?;

        Ok(())
    }
}

fn read_block_seq(file: &mut File, block: usize) -> Result<u64> {
    let mut buf: [u8; SEQ_SIZE] = [0; _];
    file.read_exact_at(&mut buf, (block * BLOCK_SIZE) as u64)?;
    Ok(u64::from_ne_bytes(buf))
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
    data_files: Arc<RwLock<Vec<DataFile>>>,

    current: File,
    file_no: u64,
    block_buf: Vec<u8>,
    block_pos: usize,
    next_seq: u64,
}

impl SeqLogReader {
    /// Return the next entry.
    pub fn next(&mut self) -> Result<Option<&[u8]>> {
        // check any entry in current block
        if let Some(len) = parse_entry_len(&self.block_buf[self.block_pos..]) {
            // yes, happy path
            self.next_seq += 1;
            self.block_pos += LEN_SIZE + len;
            return Ok(Some(&self.block_buf[self.block_pos - len..self.block_pos]));
        }

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
            let data_files = self.data_files.read().unwrap();
            let Some(data_file) = data_files
                .iter()
                .rev()
                .find(|f| f.file_no == self.file_no + 1)
            else {
                // no more file, roll back the block_buf and return EOF
                if buf_base != 0 {
                    self.block_buf.truncate(buf_base);
                }
                return Ok(None);
            };

            // open new file
            self.file_no += 1;
            self.current = File::open(&data_file.path)?;

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
        let (file, file_no, block_pos) = seek_seq(&self.data_files, seq, &mut self.block_buf)?;

        // unlock original file
        let data_files = self.data_files.read().unwrap();
        let data_file = data_files
            .iter()
            .rev()
            .find(|&f| f.file_no == self.file_no)
            .unwrap();
        data_file.refers.fetch_sub(1, Ordering::Relaxed); // unlock the file

        // update
        self.block_pos = block_pos;
        self.current = file;
        self.file_no = file_no;
        self.next_seq = seq;
        Ok(())
    }
}

// seek the sequence number in all data files
fn seek_seq(
    arc_data_files: &Arc<RwLock<Vec<DataFile>>>,
    seq: u64,
    block_buf: &mut Vec<u8>,
) -> Result<(File, u64, usize)> {
    // if seq > self.main.next_seq {
    //     return Ok(false);
    // }

    // locate the file
    let data_files = arc_data_files.read().unwrap();
    let Some(data_file) = data_files.iter().rev().find(|&f| f.start_seq <= seq) else {
        return Err(Error::new(ErrorKind::NotFound, "seq is expired"));
    };
    data_file.refers.fetch_add(1, Ordering::Relaxed); // lock the file

    // copy info to unlock the data_files ASAP
    let start_seq = data_file.start_seq;
    let path = data_file.path.clone();
    let file_no = data_file.file_no;
    drop(data_files);

    // seek the seq in the file
    let (file, block_pos) = match seek_seq_in_file(&path, start_seq, seq, block_buf) {
        Ok((file, block_pos)) => (file, block_pos),
        Err(err) => {
            // roll back the lock
            let data_files = arc_data_files.read().unwrap();
            let data_file = data_files
                .iter()
                .rev()
                .find(|&f| f.start_seq <= seq)
                .unwrap();
            data_file.refers.fetch_sub(1, Ordering::Relaxed); // unlock the file
            return Err(err);
        }
    };

    Ok((file, file_no, block_pos))
}

// seek the sequence number in one data file
fn seek_seq_in_file(
    path: &Path,
    start_seq: u64,
    seq: u64,
    block_buf: &mut Vec<u8>,
) -> Result<(File, usize)> {
    let mut file = File::open(&path)?;

    // locate block in file
    let (block_index, block_seq) = locate_block(&mut file, start_seq, seq)?;

    // locate entry in block
    let block_pos = locate_entry(&mut file, block_buf, block_index, block_seq, seq)?;

    Ok((file, block_pos))
}

// locate the block in the file
fn locate_block(file: &mut File, start_seq: u64, seq: u64) -> Result<(usize, u64)> {
    assert!(start_seq <= seq);

    let file_len = file.metadata()?.len() as usize;
    if file_len <= BLOCK_SIZE {
        return Ok((0, start_seq));
    }

    let block_count = (file_len + BLOCK_SIZE - 1) / BLOCK_SIZE;

    let last_block_seq = read_block_seq(file, block_count - 1)?;

    // guess a block index
    let seqs_per_block = (last_block_seq - start_seq) as usize / (block_count - 1);
    let guess_block_index = (seq - start_seq) as usize / seqs_per_block;

    // search the block around the guessed-block
    let guess_block_seq = read_block_seq(file, guess_block_index)?;
    if guess_block_seq > seq {
        // search backward
        for i in (0..guess_block_index).rev() {
            let blk_seq = read_block_seq(file, i)?;
            if blk_seq <= seq {
                return Ok((i, blk_seq));
            }
        }
    } else if guess_block_seq < seq {
        // search forward
        let mut last_block_seq = guess_block_seq;
        for i in guess_block_index + 1..block_count {
            let blk_seq = read_block_seq(file, i)?;
            if blk_seq > seq {
                return Ok((i - 1, last_block_seq));
            }
            last_block_seq = blk_seq;
        }
    } else {
        return Ok((guess_block_index, guess_block_seq));
    }

    unreachable!("no block found");
}

// locate the entry in the block
fn locate_entry(
    file: &mut File,
    block_buf: &mut Vec<u8>,
    block_index: usize,
    block_seq: u64,
    seq: u64,
) -> Result<usize> {
    block_buf.resize(BLOCK_SIZE, 0); // TODO optimize

    // read block from file
    file.seek(SeekFrom::Start((block_index * BLOCK_SIZE) as u64))?;
    let block_len = file.read(block_buf)?;
    block_buf.truncate(block_len);

    // parse entries
    let mut pos = SEQ_SIZE;
    for _ in 0..seq - block_seq {
        let Some(len) = parse_entry_len(&block_buf[pos..]) else {
            return Err(Error::new(ErrorKind::NotFound, "seq is unreal"));
        };
        pos += LEN_SIZE + len;
    }
    Ok(pos)
}

// load all data files
fn load_data_files(dir: &Path) -> Result<Vec<DataFile>> {
    let mut files = Vec::new();
    let mut seq_buf: [u8; SEQ_SIZE] = [0; _];

    for entry in fs::read_dir(dir)? {
        let path = entry?.path().to_path_buf();
        let fname = path
            .file_name()
            .expect("invalid filename")
            .to_str()
            .expect("fail to parse filename");
        if !fname.ends_with(SUBFIX) {
            continue;
        }
        let Ok(file_no) = fname[..fname.len() - SUBFIX.len()].parse() else {
            continue;
        };

        let mut file = File::open(&path)?;
        file.read_exact(&mut seq_buf)?;
        let start_seq = u64::from_ne_bytes(seq_buf);

        files.push(DataFile {
            path,
            start_seq,
            file_no,
            refers: AtomicUsize::new(0),
        });
    }

    // sort by file_no
    files.sort_by_key(|af| af.file_no);

    // check the file_no is consecutive
    if !files.windows(2).all(|a| a[0].file_no + 1 == a[1].file_no) {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "file NO is not consecutive",
        ));
    }

    // check the start_seq is increasing
    if !files.windows(2).all(|a| a[0].start_seq < a[1].start_seq) {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "start_seq is not increasing",
        ));
    }

    Ok(files)
}

// read last sequence number
fn read_file_info(fname: &Path) -> Result<(u64, usize)> {
    let mut file = File::open(fname)?;

    let meta = file.metadata()?;
    let len = meta.len() as usize;

    // read block into memory
    let mut block = Vec::new();
    file.seek(SeekFrom::Start((len / BLOCK_SIZE * BLOCK_SIZE) as u64))?;
    file.read_to_end(&mut block)?;

    if block.len() < SEQ_SIZE {
        panic!("invalid block header {}", block.len());
    }

    // parse block header: start seq
    let mut next_seq = parse_seq(&block);
    dbg!(next_seq);

    // parse entries
    let mut entry = &block[SEQ_SIZE..];
    while let Some(len) = parse_entry_len(entry) {
        entry = &entry[LEN_SIZE + len..];
        next_seq += 1;
    }

    Ok((next_seq, len))
}

// build data file name
fn file_name(file_no: u64) -> String {
    format!("{:08}{}", file_no, SUBFIX)
}

// parse next entry's length, if any
fn parse_entry_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < LEN_SIZE {
        None
    } else {
        let len = u16::from_ne_bytes(buf[0..LEN_SIZE].try_into().unwrap());
        if len == 0 { None } else { Some(len as usize) }
    }
}

// parse sequence number
fn parse_seq(buf: &[u8]) -> u64 {
    u64::from_ne_bytes(buf[0..SEQ_SIZE].try_into().unwrap())
}
