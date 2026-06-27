use seqlog::{Result, SeqLog};

#[test]
fn basic() -> Result<()> {
    let store = SeqLog::open("target/tests-store")?;

    // too big seq
    assert!(store.reader(store.next_seq() + 1, false).is_err());

    // too small seq
    assert!(store.reader(0, false).is_err());

    let mut reader1 = store.reader(store.next_seq() - 1000, false)?;
    let mut reader2 = store.reader(store.next_seq() - 1000, false)?;

    while let Some(entry) = reader1.next()? {
        assert_eq!(entry, reader2.next()?.unwrap());
    }
    assert_eq!(reader2.next()?, None);
    Ok(())
}
