use std::fs::{self, File};
use std::io::{Error, IoSlice, Read, Result, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const LOCK_FILE: &'static str = "LOCK";
const SUBFIX: &'static str = ".seqlog";
const BLOCK_SIZE: usize = 64 * 1024;

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

            let seq_buf: [u64; 1] = [1];
            let fname = format!("{:08}{}", 1, SUBFIX);
            fs::write(path.join(fname), from_slice_u64(&seq_buf))?;
        }

        let lock = File::open(path.join(LOCK_FILE))?;
        lock.try_lock()?;

        let (current_file_no, current_path) = locate_last_file(path)?;

        let (next_seq, block_left) = read_file_info(path, &current_path)?;

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
    //   +2-+2-+
    //   |C |L |
    //   +2C------------------+
    //   | len, ...           |
    //   +---------------------------------+
    //   | entry, ...                      |
    //   +---------------------------------+
    //
    // - C: count of entries in this segment
    // - L: sum of lengths
    //
    // so the length of this segment is: 4 + 4 + 4*cnt + slen
    pub fn append<T>(&mut self, entries: &[T]) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        let mut current_count = 0; // of current segment
        let mut current_total_len = 0; // of current segment
        let mut start_seqs = Vec::new(); // if new block
        let mut headers = Vec::new(); // segment header: C + L
        let mut lengths = Vec::with_capacity(entries.len()); // lengths

        let mut io_slices = Vec::with_capacity(entries.len() + 2); // final input vecter
        let mut block_index = Vec::new(); // index of io_slices for block
        let mut segment_index = Vec::new(); // index of io_slices for segment

        const ZEROS_BUF: [u8; 4] = [0; 4];
        let dummy = IoSlice::new(&ZEROS_BUF);

        // check if the block-left is too small
        if self.block_left < 2 + 2 {
            // padding the block
            io_slices.push(IoSlice::new(&ZEROS_BUF[..self.block_left]));

            // new block
            start_seqs.push(self.next_seq);
            block_index.push(io_slices.len());
            io_slices.push(dummy); // hold the place

            self.block_left = BLOCK_SIZE - 8;
        }

        // new segment
        segment_index.push((io_slices.len(), lengths.len(), 0));
        io_slices.push(dummy); // hold the place

        self.block_left -= 2 + 2; // reserve for segment header: C + L

        // iterate entries
        for entry in entries.iter() {
            let entry = entry.as_ref();
            let len = entry.len();

            if len > BLOCK_SIZE / 2 {
                return Err(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "too long entry",
                ));
            }

            if 2 + len > self.block_left {
                // need a new block

                // prepare the header for current segment
                headers.push(current_count as u16);
                headers.push(current_total_len as u16);
                segment_index.last_mut().unwrap().2 = lengths.len();

                // clear for new segment in new block
                current_total_len = 0;
                current_count = 0;

                // padding the block, by the current entry
                if len >= self.block_left {
                    io_slices.push(IoSlice::new(&entry[..self.block_left]));
                } else {
                    io_slices.push(IoSlice::new(&entry));
                    io_slices.push(IoSlice::new(&ZEROS_BUF[..1])); // just 1 byte
                }

                // new block
                start_seqs.push(self.next_seq);
                block_index.push(io_slices.len());
                io_slices.push(dummy); // hold the place

                // new segment in new block
                segment_index.push((io_slices.len(), lengths.len(), 0));
                io_slices.push(dummy); // hold the place

                self.block_left = BLOCK_SIZE - 8 - 2 - 2;
            }

            // encode the entry
            self.next_seq += 1;
            self.block_left -= 2 + len;
            current_count += 1;
            current_total_len += len;
            lengths.push(len as u16);
            io_slices.push(IoSlice::new(entry));
        }

        // fix the io_slices for block header
        for (blki, si) in block_index.into_iter().enumerate() {
            io_slices[si] = IoSlice::new(from_slice_u64(&start_seqs[blki..blki + 1]));
        }

        // fix the io_slices for segment header
        segment_index.last_mut().unwrap().2 = lengths.len();
        for (si, len_start, len_end) in segment_index.into_iter() {
            io_slices[si] = IoSlice::new(from_slice_u16(&lengths[len_start..len_end]));
        }

        // finally, write into file and sync
        self.current.write_vectored(&io_slices)?;
        self.current.sync_data()?;
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

fn read_file_info(dir: &Path, fname: &Path) -> Result<(u64, usize)> {
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

    // parse segments
    let mut segment = &block[8..];
    while !segment.is_empty() {
        let count = u16::from_le_bytes(segment[0..2].try_into().unwrap());
        let total_len = u16::from_le_bytes(segment[2..4].try_into().unwrap());

        next_seq += count as u64;

        let raw_len = 2 + 2 + count as usize * 2 + total_len as usize;
        segment = &segment[raw_len..];
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
fn from_slice_u64(buf: &[u64]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            buf.as_ptr() as *const u8,
            buf.len() * std::mem::size_of::<u64>(),
        )
    }
}
