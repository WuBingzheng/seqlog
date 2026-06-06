use std::fs::{self, File};
use std::io::{Error, ErrorKind, IoSlice, Read, Result, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::usize;

const LOCK_FILE: &'static str = "LOCK";
const SUBFIX: &'static str = ".seqlog";

const BLOCK_SIZE: usize = 64 * 1024;
const ENTRY_MAX_LEN: usize = BLOCK_SIZE - 8 - 2;

const FIRST_SEQ: u64 = 1;

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct ArchiveFile {
    file_no: u64,
    // size: usize,
    start_seq: u64,
    path: PathBuf,
}

pub struct SeqLog {
    path: PathBuf,

    // config
    rotate_size: usize,
    rotate_count: usize,

    lock: File,
    archives: Vec<ArchiveFile>,
    current: File,
    current_file_no: u64,
    current_file_size: usize,
    block_left: usize,
    start_seq: u64,
    next_seq: u64,
    current_path: PathBuf,
}

impl SeqLog {
    pub fn rotate(&mut self) -> Result<()> {
        // old
        self.archives.push(ArchiveFile {
            file_no: self.current_file_no,
            start_seq: self.start_seq,
            path: self.current_path.clone(),
            // size: self.current_file_size,
        });

        // new
        self.current_file_no += 1;
        let fname = format!("{:08}{}", self.current_file_no, SUBFIX);
        self.current_path = self.path.join(fname);

        self.current = File::create_new(&self.current_path)?;
        self.current.write(from_single_u64(&self.next_seq))?;

        self.start_seq = self.next_seq;
        self.current_file_size = 8;
        self.block_left = BLOCK_SIZE - 8;

        // expire
        while self.archives.len() > self.rotate_count {
            let file = self.archives.remove(0);
            fs::remove_file(file.path)?;
        }

        Ok(())
    }

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

        let mut archives = load_archive_files(path)?;

        let ArchiveFile {
            file_no: current_file_no,
            start_seq,
            path: current_path,
        } = archives.pop().unwrap();

        let (next_seq, current_file_size) = read_file_info(&current_path)?;
        let block_left = BLOCK_SIZE - (current_file_size % BLOCK_SIZE);
        dbg!(next_seq);

        let current = File::options().append(true).open(&current_path)?;

        Ok(Self {
            path: path.to_path_buf(),
            rotate_size: 1024 * 1024 * 1024, // 1G
            rotate_count: 20,
            lock,
            archives,
            current,
            current_file_no,
            current_file_size,
            block_left,
            start_seq,
            next_seq,
            current_path,
        })
    }

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

        const ZEROS: [u8; 2] = [0; 2];
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

            if 2 + len > self.block_left {
                // we need a new block

                // padding the block
                if self.block_left <= 2 {
                    bufs.push(IoSlice::new(&ZEROS[..self.block_left]));
                } else {
                    // push 2 zeros to indicate the end of block
                    bufs.push(IoSlice::new(&ZEROS[..2]));
                    // pad the remaining by this entry which is long enough
                    bufs.push(IoSlice::new(&entry[..self.block_left - 2]));
                }

                // new block
                blocks.push((bufs.len(), self.next_seq));
                bufs.push(dummy); // hold the place for block header

                self.block_left = BLOCK_SIZE - 8;
            }

            // encode the length
            lengths.push((bufs.len(), len as u16));
            bufs.push(dummy); // hold the place for length

            // encode the entry
            bufs.push(IoSlice::new(entry));

            self.next_seq += 1;
            self.block_left -= 2 + len;
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
}

fn load_archive_files(dir: &Path) -> Result<Vec<ArchiveFile>> {
    let mut files = Vec::new();
    let mut seq_buf: [u8; 8] = [0; _];

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

        files.push(ArchiveFile {
            path,
            start_seq,
            file_no,
        });
    }

    // sort by file_no
    files.sort();

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

    if block.len() < 8 {
        panic!("invalid block header {}", block.len());
    }

    // parse block header: start seq
    let mut next_seq = u64::from_le_bytes(block[..8].try_into().unwrap());
    dbg!(next_seq);

    // parse entries
    let mut entry = &block[8..];
    while entry.len() > 2 {
        let len = u16::from_le_bytes(entry[0..2].try_into().unwrap());
        if len == 0 {
            break;
        }
        entry = &entry[2 + len as usize..];

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
