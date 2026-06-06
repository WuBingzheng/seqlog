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

const FIRST_SEQ: u64 = 1;

struct SeqLogFile {
    file_no: u64,
    start_seq: u64,
    path: PathBuf,
    refers: AtomicUsize,
}

pub struct SeqLog {
    path: PathBuf,

    // config
    rotate_size: usize,
    rotate_count: usize,

    _lock: File,
    files: Arc<RwLock<Vec<SeqLogFile>>>,
    current: File,
    current_file_size: usize,
    block_left: usize,
    next_seq: u64,
}

impl SeqLog {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            fs::create_dir_all(path)?;

            File::create(path.join(LOCK_FILE))?;

            let fname = format!("{:08}{}", 1, SUBFIX);
            fs::write(path.join(fname), from_single_u64(&FIRST_SEQ))?;
        }

        let lock = File::open(path.join(LOCK_FILE))?;
        lock.try_lock()?;

        let files = load_files(path)?;

        let current_info = files.last().unwrap();

        let (next_seq, current_file_size) = read_file_info(&current_info.path)?;
        let block_left = BLOCK_SIZE - (current_file_size % BLOCK_SIZE);
        dbg!(next_seq);

        let current = File::options().append(true).open(&current_info.path)?;

        Ok(Self {
            path: path.to_path_buf(),
            rotate_size: 1024 * 1024 * 1024, // 1G
            rotate_count: 20,
            _lock: lock,
            files: Arc::new(RwLock::new(files)),
            current,
            current_file_size,
            block_left,
            next_seq,
        })
    }

    /// Configrate rotation.
    pub fn set_rotate(&mut self, size: usize, count: usize) {
        self.rotate_size = if size == 0 { usize::MAX } else { size };
        self.rotate_count = if count == 0 { usize::MAX } else { count };
    }

    // block header:
    //   +8--------+
    //   | seq     |
    //   +---------+
    //
    // entry format:
    //   +2--+----------------+
    //   |len|entry           |
    //   +---+----------------+
    pub fn append<T>(&mut self, entries: &[T]) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        let mut blocks = Vec::new(); // block header: start seq
        let mut lengths = Vec::with_capacity(entries.len()); // lengths for every entries
        let mut bufs = Vec::with_capacity(entries.len() + 2);

        let origin_block_left = self.block_left;

        const ZEROS: [u8; LEN_SIZE] = [0; _];
        let dummy = IoSlice::new(&ZEROS);

        for entry in entries.iter() {
            let entry = entry.as_ref();
            let len = entry.len();

            if len == 0 {
                continue;
            }
            if len > ENTRY_MAX_LEN {
                return Err(Error::new(ErrorKind::InvalidData, "too long entry"));
            }

            if LEN_SIZE + len > self.block_left {
                // we need a new block

                // padding the block
                if self.block_left <= LEN_SIZE {
                    bufs.push(IoSlice::new(&ZEROS[..self.block_left]));
                } else {
                    // push 2 zeros to indicate the end of block
                    bufs.push(IoSlice::new(&ZEROS[..LEN_SIZE]));
                    // pad the remaining by this entry which is long enough
                    bufs.push(IoSlice::new(&entry[..self.block_left - LEN_SIZE]));
                }

                // new block
                blocks.push((bufs.len(), self.next_seq));
                bufs.push(dummy); // hold the place for block header

                self.block_left = BLOCK_SIZE - SEQ_SIZE;
            }

            // encode the length
            lengths.push((bufs.len(), len as u16));
            bufs.push(dummy); // hold the place for length

            // encode the entry
            bufs.push(IoSlice::new(entry));

            self.next_seq += 1;
            self.block_left -= LEN_SIZE + len;
        }

        // fix the bufs for segments header: lengths
        for (i, len) in lengths.iter() {
            bufs[*i] = IoSlice::new(from_single_u16(len));
        }

        // fix the bufs for blocks header: start_seg
        for (i, seq) in blocks.iter() {
            bufs[*i] = IoSlice::new(from_single_u64(seq));
        }

        // finally, write into file
        let mut total_len = blocks.len() * BLOCK_SIZE + origin_block_left - self.block_left;
        self.current_file_size += total_len;
        loop {
            match self.current.write_vectored(&bufs) {
                Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "write zero")),
                Ok(n) if n == total_len => break, // success!
                Ok(n) => {
                    // partial, try again
                    IoSlice::advance_slices(&mut bufs.as_mut_slice(), n);
                    total_len -= n;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        // check rotation
        if self.current_file_size >= self.rotate_size {
            self.rotate()?;
        }

        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.current.sync_data()
    }

    // pub fn new_scanner(&self, start_seq: u64) -> Option<SeqLogScanner> {
    //     if start_seq > self.next_seq {
    //         return None;
    //     }
    //     if start_seq >= self.start_seq {
    //     } else {
    //         let file_infos = self.file_infos.read().unwrap();
    //         for info in file_infos.iter().rev() {
    //             if info.start_seq <= start_seq {
    //                 break;
    //             }
    //         }
    //     }

    //     SeqLogScanner {
    //         file_infos: self.file_infos.clone(),
    //     }
    // }

    pub fn rotate(&mut self) -> Result<()> {
        // open new file
        let last_file_no = self.files.read().unwrap().last().unwrap().file_no;
        let new_file_no = last_file_no + 1;
        let fname = format!("{:08}{}", new_file_no, SUBFIX);
        let path = self.path.join(fname);

        self.current = File::create_new(&path)?;
        self.current.write(from_single_u64(&self.next_seq))?;

        self.current_file_size = SEQ_SIZE;
        self.block_left = BLOCK_SIZE - SEQ_SIZE;

        // save new file info
        let new_file = SeqLogFile {
            file_no: new_file_no,
            start_seq: self.next_seq,
            path,
            refers: AtomicUsize::new(0),
        };

        let mut files = self.files.write().unwrap();
        files.push(new_file);

        // expire
        while files.len() > self.rotate_count {
            if files[0].refers.load(Ordering::Relaxed) > 0 {
                // this file is in used
                break;
            }
            let file = files.remove(0);
            fs::remove_file(file.path)?;
        }
        Ok(())
    }
}

fn read_block_seq(file: &mut File, block: usize) -> Result<u64> {
    let mut buf: [u8; SEQ_SIZE] = [0; _];
    file.read_exact_at(&mut buf, (block * BLOCK_SIZE) as u64)?;
    Ok(u64::from_le_bytes(buf))
}

pub struct SeqLogScanner {
    files: Arc<RwLock<Vec<SeqLogFile>>>,

    next_seq: u64,
    current: File,
    file_no: u64,
    block_buf: Vec<u8>,
    block_pos: usize,
}

impl SeqLogScanner {
    pub fn reset_seq(&mut self, seq: u64) -> Result<()> {
        // if seq > self.main.next_seq {
        //     return Ok(false);
        // }

        // locate the file
        let files = self.files.read().unwrap();
        let Some(seqlog_file) = files.iter().rev().find(|&f| f.start_seq <= seq) else {
            return Err(Error::new(ErrorKind::NotFound, "seq is expired"));
        };
        seqlog_file.refers.fetch_add(1, Ordering::Relaxed); // lock the file

        // copy info to unlock ASAP
        let start_seq = seqlog_file.start_seq;
        let path = seqlog_file.path.clone();
        let file_no = seqlog_file.file_no;
        drop(files);

        let mut file = File::open(&path)?;

        // locate block in file
        let (block_index, block_seq) = locate_block(&mut file, start_seq, seq)?;

        // locate entry in block
        self.block_pos = locate_entry(&mut file, &mut self.block_buf, block_index, block_seq, seq)?;

        // done
        self.current = file;
        self.file_no = file_no;
        self.next_seq = seq;
        Ok(())
    }

    pub fn next(&mut self) -> Result<Option<&[u8]>> {
        todo!()
    }
}

fn locate_block(file: &mut File, start_seq: u64, seq: u64) -> Result<(usize, u64)> {
    assert!(start_seq <= seq);

    let file_len = file.metadata()?.len() as usize;
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

fn locate_entry(
    file: &mut File,
    block_buf: &mut Vec<u8>,
    block_index: usize,
    mut next_seq: u64,
    seq: u64,
) -> Result<usize> {
    block_buf.resize(BLOCK_SIZE, 0); // TODO optimize

    // read block from file
    file.seek(SeekFrom::Start((block_index * BLOCK_SIZE) as u64))?;
    let block_len = file.read(block_buf)?;
    block_buf.truncate(block_len);

    // parse entries
    let mut pos = SEQ_SIZE;
    while pos < block_len - LEN_SIZE {
        let len = u16::from_le_bytes(block_buf[pos..pos + LEN_SIZE].try_into().unwrap());
        if len == 0 {
            break;
        }

        next_seq += 1;
        if next_seq == seq {
            return Ok(pos);
        }

        pos += LEN_SIZE + len as usize;
    }

    return Err(Error::new(ErrorKind::NotFound, "seq is unreal"));
}

fn load_files(dir: &Path) -> Result<Vec<SeqLogFile>> {
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
        let start_seq = u64::from_le_bytes(seq_buf);

        files.push(SeqLogFile {
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
    let mut next_seq = u64::from_le_bytes(block[..SEQ_SIZE].try_into().unwrap());
    dbg!(next_seq);

    // parse entries
    let mut entry = &block[SEQ_SIZE..];
    while entry.len() > LEN_SIZE {
        let len = u16::from_le_bytes(entry[0..LEN_SIZE].try_into().unwrap());
        if len == 0 {
            break;
        }
        entry = &entry[LEN_SIZE + len as usize..];

        next_seq += 1;
    }

    Ok((next_seq, len))
}

fn from_single_u16(r: &u16) -> &[u8] {
    unsafe { std::slice::from_raw_parts(r as *const u16 as *const u8, 2) }
}

fn from_single_u64(r: &u64) -> &[u8] {
    unsafe { std::slice::from_raw_parts(r as *const u64 as *const u8, 8) }
}
