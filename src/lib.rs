use std::fs::{self, File};
use std::io::{Error, IoSlice, Read, Result, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const LOCK_FILE: &'static str = "LOCK";
const SUBFIX: &'static str = ".seqlog";

// encode the length as u16
const LAST_LEN_MASK: u16 = 0x7FFF;
const LAST_LEN_FLAG: u16 = 0x8000;

const ENTRY_MAX_LEN: usize = LAST_LEN_MASK as usize; // make sure: this <= LAST_LEN_MASK
const BLOCK_SIZE: usize = 64 * 1024; // make sure: this >= ENTRY_MAX_LEN + 8 + 2

pub struct Config {
    pub rotation: usize,
    pub retention: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rotation: 1024 * 1024 * 1024, // 1GB
            retention: 10,
        }
    }
}

impl Config {}

pub struct SeqLog {
    path: PathBuf,
    config: Config,
    lock: File,
    current: File,
    current_file_no: u64,
    block_left: usize,
    next_seq: u64,
}

impl SeqLog {
    pub fn rotate(&mut self) -> Result<()> {
        self.current_file_no += 1;
        let fname = format!("{:08}{}", self.current_file_no, SUBFIX);
        self.current = File::create_new(self.path.join(fname))?;
        Ok(())
    }

    pub fn open<P: AsRef<Path>>(path: P, config: Config) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            std::fs::create_dir_all(path)?;

            File::create(path.join(LOCK_FILE))?;

            let fname = format!("{:08}{}", 1, SUBFIX);
            fs::write(path.join(fname), from_single_u64(&1))?;
        }

        let lock = File::open(path.join(LOCK_FILE))?;
        lock.try_lock()?;

        let (current_file_no, current_path) = locate_last_file(path)?;

        let (next_seq, block_left) = read_file_info(&current_path)?;
        dbg!(next_seq);

        let current = File::options().append(true).open(&current_path)?;

        Ok(Self {
            path: path.to_path_buf(),
            config,
            lock,
            current,
            current_file_no,
            block_left,
            next_seq,
        })
    }

    // block header:
    //   +8--------+
    //   | seq     |
    //   +---------+
    //
    // segment format:
    //   +--------------------+
    //   | len, ...           |
    //   +---------------------------------+
    //   | entry, ...                      |
    //   +---------------------------------+
    //
    // The @len is 16-bit, and the last one is set the highest bit.
    pub fn append<T>(&mut self, entries: &[T], sync: bool) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        let mut start_seqs = Vec::new(); // block header: start seq
        let mut lengths = Vec::with_capacity(entries.len()); // lengths for segments

        let mut io_slices = Vec::with_capacity(entries.len() + 2);
        let mut block_index = Vec::new(); // index of io_slices for block
        let mut segment_index = Vec::new(); // index of io_slices for segment

        const DUMMY: [u8; 1] = [0];
        let dummy = IoSlice::new(&DUMMY);

        // new segment
        segment_index.push((io_slices.len(), lengths.len(), lengths.len()));
        io_slices.push(dummy); // hold the place for lengths

        // iterate entries
        for entry in entries.iter() {
            let entry = entry.as_ref();
            let len = entry.len();

            if len == 0 {
                continue;
            }
            if len > ENTRY_MAX_LEN {
                return Err(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "too long entry",
                ));
            }

            if 2 + len > self.block_left {
                // we need a new block

                // padding the block, by the current entry
                if len >= self.block_left {
                    io_slices.push(IoSlice::new(&entry[..self.block_left]));
                } else {
                    io_slices.push(IoSlice::new(&entry));
                    io_slices.push(IoSlice::new(&entry[..1])); // just 1 more byte
                }

                // close current segment
                if let Some(last_len) = lengths.last_mut() {
                    *last_len |= LAST_LEN_FLAG;
                    segment_index.last_mut().unwrap().2 = lengths.len();
                }

                // new block
                start_seqs.push(self.next_seq);
                block_index.push(io_slices.len());
                io_slices.push(dummy); // hold the place for block header

                // new segment in new block
                segment_index.push((io_slices.len(), lengths.len(), 0));
                io_slices.push(dummy); // hold the place for lengths

                self.block_left = BLOCK_SIZE - 8;
            }

            // encode the entry
            self.next_seq += 1;
            self.block_left -= 2 + len;
            lengths.push(len as u16);
            io_slices.push(IoSlice::new(entry));
        }

        // close the last segment
        if let Some(last_len) = lengths.last_mut() {
            *last_len |= LAST_LEN_FLAG;
            segment_index.last_mut().unwrap().2 = lengths.len();
        }

        // fix the io_slices for segments header: lengths
        for (slci, len_start, len_end) in segment_index.into_iter() {
            io_slices[slci] = IoSlice::new(from_slice_u16(&lengths[len_start..len_end]));
        }

        // fix the io_slices for blocks header: start_seg
        for (slci, seqr) in block_index.into_iter().zip(start_seqs.iter()) {
            io_slices[slci] = IoSlice::new(from_single_u64(seqr));
        }

        // finally, write into file and sync
        self.current.write_vectored(&io_slices)?;
        if sync {
            self.current.sync_data()?;
        }
        Ok(())
    }

    pub fn scan(&mut self, _start: u64, _end: u64) -> Result<()> {
        Ok(())
    }
}

fn locate_last_file(dir: &Path) -> Result<(u64, PathBuf)> {
    let mut max_no = 0;
    let mut last_path = PathBuf::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let fname = path
            .file_name()
            .expect("invalid filename")
            .to_str()
            .expect("fail to parse filename");
        if fname == LOCK_FILE {
            continue;
        }
        if !fname.ends_with(SUBFIX) {
            continue; // TODO
        }
        let seq: u64 = fname[..fname.len() - SUBFIX.len()].parse().unwrap();
        if seq >= max_no {
            max_no = seq;
            last_path = path;
        }
    }
    Ok((max_no, last_path))
}

fn read_file_info(fname: &Path) -> Result<(u64, usize)> {
    let mut file = File::open(fname)?;

    let meta = file.metadata()?;
    let len = meta.len() as usize;
    let block_left = BLOCK_SIZE - (len % BLOCK_SIZE);

    // read block into memory
    let mut block = Vec::new();
    file.seek(SeekFrom::Start((len / BLOCK_SIZE * BLOCK_SIZE) as u64))?;
    file.read_to_end(&mut block)?;

    if block.len() < 8 {
        panic!("invalid block header {}", block.len());
    }

    // parse block header: seq
    let mut next_seq = u64::from_le_bytes(block[..8].try_into().unwrap());
    dbg!(next_seq);

    // parse segments
    let mut segment = &block[8..];
    while !segment.is_empty() {
        let mut sum_len = 0;
        loop {
            let len = u16::from_le_bytes(segment[0..2].try_into().unwrap());
            segment = &segment[2..];

            next_seq += 1;
            if len & LAST_LEN_FLAG == 0 {
                sum_len += len;
            } else {
                sum_len += len & LAST_LEN_MASK;
                break;
            }
        }

        segment = &segment[sum_len as usize..];
    }

    Ok((next_seq, block_left))
}

fn from_slice_u16(buf: &[u16]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            buf.as_ptr() as *const u8,
            buf.len() * std::mem::size_of::<u16>(),
        )
    }
}

fn from_single_u64(r: &u64) -> &[u8] {
    unsafe { std::slice::from_raw_parts(r as *const u64 as *const u8, 8) }
}
