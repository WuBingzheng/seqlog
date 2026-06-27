An embedded append-only sequence log store.

SeqLog is designed for workloads where records are identified by monotonically
increasing sequence numbers and are primarily accessed in sequence order.
Typical use cases include event storage, write-ahead logs (WAL), market data
capture, message persistence, and other append-heavy workloads.

SeqLog can be viewed as a specialized key-value store, where the Key is
a monotonically increasing sequence number (`u64`), and the Value is
arbitrary bytes (`[u8]`).


# Features

- Append-only writes,
- Sequence-based seeking and sequential reads,
- Truncation from either the head or tail,
- Cross-thread disk synchronization to avoid blocking,
- No in-place updates;

and

- High performance and throughput. The benchmark is in processing,
- Predictable and compact storage layout.


# Usage

```rust
use seqlog::{SeqLog, Result};

fn example() -> Result<()> {
    // Create a SeqLog instance, with sequence number starting from 100.
    // Use `open()` instead to open an existing SeqLog instance.
    let mut store = SeqLog::create("target/example-store/", 100)?;

    // Append entries in batch.
    let entries = vec![
        "Hello, world!",
        "The value could be arbitrary bytes.",
        "Live & Learn!",
    ];
    store.append(&entries)?;

    // Create a reader to read sequentially, from sequence number 102.
    // You can create multiple readers, with independent cursors.
    let mut reader = store.reader(102, false)?;
    assert_eq!(reader.next()?, Some(entries[2].as_bytes())); // read 102
    assert_eq!(reader.next()?, None); // EOF
    Ok(())
}

// example().unwrap();
```

This is just the basic usage and does not show cross-thread disk
synchronization and others. See examples for more details.


# Storage Layout

This section describes implementation details that are not required for
normal use of SeqLog.

After experimenting with several layouts (fixed-size blocks, inline indexes,
and batch-level checksums), I chose current simple one. It is not the most
optimal in every aspect, but overall it offers the fewest trade-offs and
drawbacks.

A SeqLog instance is stored in a directory containing a `LOCK` file, multiple
data files, and their corresponding index files.

Both data and index files are named after the sequence number of their first
entry, with different extension, `.data` or `.index`.

Let's see the directory created by above example:

```text
$ ls target/example-store
00000000000000000100.data  00000000000000000100.index  LOCK
```

A data file consists of entries. Each entry consists of length (2 bytes),
checksum (2 bytes), and payload. The maximum payload length is 65535.

This is the data file of above example. I use `[]` to mark the length of each entry:

```text
$ hexdump -C target/example-store/00000000000000000100.data
00000000 [0d 00]e6 c6 48 65 6c 6c  6f 2c 20 77 6f 72 6c 64  |....Hello, world|
00000010  21[23 00]e7 86 54 68 65  20 76 61 6c 75 65 20 63  |!#...The value c|
00000020  6f 75 6c 64 20 62 65 20  61 72 62 69 74 72 61 72  |ould be arbitrar|
00000030  79 20 62 79 74 65 73 2e [0d 00]24 f7 4c 69 76 65  |y bytes...$.Live|
00000040  20 26 20 4c 65 61 72 6e  21                       | & Learn!|
```

An index file stores sparse indexes. One index entry (8 bytes) is created
for every 1024 data entries, recording the corresponding offset within
the data file.

Since there is less than 1024 entries in the above example, the index file
is empty.

Both the data and index formats are intentionally simple, making it
straightforward for external tools to parse.


# Comparison

This section compares SeqLog with several existing append-only storage crates.

[`commitlog`](https://docs.rs/commitlog/)

It does not provide a dedicated reader abstraction, so reads and writes
must be performed through the same handle, in the same thread.

The `append()` API writes a single record, while `flush()` is used to flush
to file. However, the implementation writes directly to `File` without
an intermediate write buffer, which means each `append()` results in a
system call and `flush()` does nothing. It feels wrong.

It does not support disk synchronization API.

[`seglog`](https://docs.rs/seglog/)

It stores data in one single fixed-size file as a ring-buffer. New records
overwrite old ones. This is simple but less flexible. It is particularly well
suited for fixed-capacity storage scenarios such as monitoring.

It does not support offloading disk synchronization to another thread.

As a side note, its name is remarkably similar to `seqlog`, just missing the
hook under the "g".

[`walcraft`](https://docs.rs/walcraft/)

It supports concurrent writes from multiple threads.

It also supports background disk synchronization, although synchronization
is performed on a timer-based schedule rather than being explicitly
controlled by the caller.
