use seqlog::{Error, Result, SeqLog};

fn open(dir: &str) -> Result<SeqLog> {
    if !std::fs::exists(dir)? {
        SeqLog::create(dir, 100)
    } else {
        SeqLog::open(dir)
    }
}

#[test]
fn basic() -> Result<()> {
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

    let next_seq0 = store.next_seq();

    // append 100*10 entries
    for _ in 0..100 {
        store.append(&entries)?;
    }

    let next_seq = store.next_seq();

    assert_eq!(next_seq, next_seq0 + 1000);

    // invalid reader
    let Err(err) = store.reader(next_seq + 1, false) else {
        panic!("expect err");
    };
    assert!(matches!(err, Error::SeqNotReached(_, _)));

    let Err(err) = store.reader(99, false) else {
        panic!("expect err");
    };
    assert!(matches!(err, Error::SeqPurged(99, _)));

    // read last 13 entries
    let mut reader = store.reader(next_seq - 13, false)?;
    for entry in entries[7..].iter() {
        assert_eq!(reader.next()?, Some(entry.as_bytes()));
    }
    for entry in entries.iter() {
        assert_eq!(reader.next()?, Some(entry.as_bytes()));
    }
    assert_eq!(reader.next()?, None);
    Ok(())
}
