use seqlog::{Error, Result, SeqLog};

fn open(dir: &str) -> Result<SeqLog> {
    if !std::fs::exists(dir)? {
        SeqLog::create(dir, 100)
    } else {
        SeqLog::open(dir)
    }
}

#[test]
fn truncate() -> Result<()> {
    // prepare
    let mut store = open("target/tests-store")?;
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

    // read last 13 entries
    let mut reader = store.reader(next_seq0 - 10, false)?;
    for entry in entries[..7].iter() {
        assert_eq!(reader.next()?, Some(entry.as_bytes()));
    }
    assert_eq!(reader.next()?, None);
    Ok(())
}
