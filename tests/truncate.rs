use seqlog::{Error, Result};

mod common;

#[test]
fn truncate() -> Result<()> {
    // prepare
    let mut store = common::open("target/tests-truncate")?;
    common::append10(&mut store)?;

    let next_seq0 = store.next_seq();

    // fail
    let Err(err) = store.truncate(next_seq0 + 1) else {
        panic!("error");
    };
    assert!(matches!(err, Error::SeqNotReached(_, _)));

    // truncate
    store.truncate(next_seq0 - 3)?;

    assert_eq!(store.next_seq(), next_seq0 - 3);

    // invalid reader
    let Err(err) = store.reader(next_seq0, false) else {
        panic!("expect err");
    };
    assert!(matches!(err, Error::SeqNotReached(_, _)));

    // read last 7 entries
    let mut reader = store.reader(next_seq0 - 10, false)?;

    let mut entry_data = common::ENTRIES.iter();
    while let Some(entry) = reader.next()? {
        assert_eq!(entry, entry_data.next().unwrap().as_bytes());
    }
    assert_eq!(reader.next()?, None);
    Ok(())
}
