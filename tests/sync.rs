use seqlog::Result;

mod common;

#[test]
fn sync() -> Result<()> {
    let mut store = common::open("target/tests-sync")?;

    // on open
    let next_seq0 = store.next_seq();
    let sync_seq0 = store.sync_seq();
    assert_eq!(next_seq0, sync_seq0);

    let mut reader1 = store.reader(next_seq0, false)?;
    let mut reader2 = store.reader(next_seq0, true)?;

    common::append10(&mut store)?;

    assert_eq!(store.next_seq(), next_seq0 + 10); // update
    assert_eq!(store.sync_seq(), sync_seq0); // no update

    // reader-1
    let mut entry_data = common::ENTRIES.iter();
    while let Some(entry) = reader1.next()? {
        assert_eq!(entry, entry_data.next().unwrap().as_bytes());
    }

    // reader-2
    assert_eq!(reader2.next()?, None);

    // sync
    store.sync()?;

    // reader-2
    let mut entry_data = common::ENTRIES.iter();
    while let Some(entry) = reader2.next()? {
        assert_eq!(entry, entry_data.next().unwrap().as_bytes());
    }

    Ok(())
}
