use seqlog::{Error, Result};

mod common;

#[test]
fn readers() -> Result<()> {
    let mut store = common::open("target/tests-store")?;

    let next_seq0 = store.next_seq();

    // invalid reader
    let Err(err) = store.reader(next_seq0 + 1, false) else {
        panic!("expect err");
    };
    assert!(matches!(err, Error::SeqNotReached(_, _)));

    // invalid reader
    let Err(err) = store.reader(99, false) else {
        panic!("expect err");
    };
    assert!(matches!(err, Error::SeqPurged(99, _)));

    // read from end
    let mut reader1 = store.reader(next_seq0, false)?;
    let mut reader2 = store.reader(next_seq0, false)?;
    let mut reader3 = store.reader(next_seq0, true)?;
    assert_eq!(reader1.next()?, None);
    assert_eq!(reader2.next()?, None);
    assert_eq!(reader3.next()?, None);

    // append 100*10 entries
    for _ in 0..100 {
        common::append10(&mut store)?;
    }

    let mut count = 0;
    while let Some(entry) = reader1.next()? {
        assert_eq!(entry, reader2.next()?.unwrap());
        count += 1;
    }
    assert_eq!(reader2.next()?, None);
    assert_eq!(count, 1000);

    // sync-reader
    assert_eq!(reader3.next()?, None);
    store.sync()?;

    reader1.reset(next_seq0)?;
    let mut count = 0;
    while let Some(entry) = reader1.next()? {
        assert_eq!(entry, reader3.next()?.unwrap());
        count += 1;
    }
    assert_eq!(reader3.next()?, None);
    assert_eq!(count, 1000);

    Ok(())
}
