use seqlog::SeqLog;
use std::time::Instant;

fn test(dir: &str) -> std::io::Result<()> {
    // open or create
    let mut store = if !std::fs::exists(dir)? {
        SeqLog::create(dir, 1)?
    } else {
        SeqLog::open(dir)?
    };

    store.set_rotate(1024 * 128, 2);

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

    // append 100*10 entries
    let start = Instant::now();
    for _ in 0..100 {
        store.append(&entries)?;
    }
    println!("append: {:?}", start.elapsed());

    // sync is slow
    let start = Instant::now();
    store.sync()?;
    println!("sync: {:?}", start.elapsed());

    // read last 10 entries
    let mut reader = store.reader(store.next_seq() - 10)?;
    while let Some(entry) = reader.next()? {
        println!("{}", std::str::from_utf8(entry).unwrap());
    }
    println!("EOF");

    // append more, and read them
    store.append(&entries[..3])?;

    while let Some(entry) = reader.next()? {
        println!("{}", std::str::from_utf8(entry).unwrap());
    }
    println!("EOF again");

    Ok(())
}

fn main() {
    test("target/example/basic").unwrap();
}
