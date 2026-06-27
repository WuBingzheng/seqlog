use seqlog::{Result, SeqLog};

fn open(dir: &str) -> Result<SeqLog> {
    if !std::fs::exists(dir)? {
        SeqLog::create(dir, 100)
    } else {
        SeqLog::open(dir)
    }
}

#[test]
fn test_sync() -> Result<()> {
    let mut store = open("target/tests-store")?;

    // on open
    let next_seq0 = store.next_seq();
    let sync_seq0 = store.sync_seq();
    assert_eq!(next_seq0, sync_seq0);

    let mut reader1 = store.reader(next_seq0, false)?;
    let mut reader2 = store.reader(next_seq0, true)?;

    // append new data
    let entries = vec![
        "hello, world!",
        "111",
        "222222",
        "333333333",
        "444444444444",
        "555555555555555",
        "666666666666666666",
        "777777777777777777777",
        "888888888888888888888888",
        "999999999999999999999999999",
    ];
    store.append(&entries)?;

    assert_eq!(store.next_seq(), next_seq0 + 10); // update
    assert_eq!(store.sync_seq(), sync_seq0); // no update

    // reader
    assert_eq!(reader1.next()?, Some(entries[0].as_bytes()));
    assert_eq!(reader2.next()?, None);

    // sync
    store.sync()?;
    assert_eq!(store.sync_seq(), sync_seq0 + 10); // update
    assert_eq!(reader2.next()?, Some(entries[0].as_bytes()));

    Ok(())
}
