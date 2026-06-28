use seqlog::{Result, SeqLog};

pub fn open(dir: &str) -> Result<SeqLog> {
    let mut store = if !std::fs::exists(dir)? {
        SeqLog::create(dir, 100)?
    } else {
        SeqLog::open(dir)?
    };

    store.set_rotate(200_000, 3);
    Ok(store)
}

pub const ENTRIES: [&str; 10] = [
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

pub fn append10(store: &mut SeqLog) -> Result<()> {
    store.append(&ENTRIES)
}
