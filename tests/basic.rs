use seqlog::Result;

mod common;

#[test]
fn basic() -> Result<()> {
    let mut store = common::open("target/tests-basic")?;

    let next_seq0 = store.next_seq();

    // append 100*10 entries
    for _ in 0..100 {
        common::append10(&mut store)?;
    }

    let next_seq = store.next_seq();
    assert_eq!(next_seq, next_seq0 + 1000);

    // read last 13 entries
    let mut reader = store.reader(next_seq - 13, false)?;

    // check first 3 entries
    for entry in common::ENTRIES.iter().skip(7) {
        assert_eq!(entry.as_bytes(), reader.next()?.unwrap());
    }

    // check last 10 entries
    let mut entry_data = common::ENTRIES.iter();
    while let Some(entry) = reader.next()? {
        assert_eq!(entry, entry_data.next().unwrap().as_bytes());
    }
    // EOF

    Ok(())
}
