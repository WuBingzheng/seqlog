use seqlog::{Config, SeqLog};

use std::time::Instant;

fn main() {
    let mut seqlog = SeqLog::open("target/example/run/", Config::default()).unwrap();

    let entries = vec![
        "111",
        "222222",
        "333333333",
        "444444444444",
        "555555555555555",
        "666666666666666666",
        "777777777777777777777",
        "888888888888888888888888",
        "999999999999999999999999999",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccc",
        "ddddddddddddddddddddddddddddddddddddddd",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        "fffffffffffffffffffffffffffffffffffffffffffff",
    ];

    let start = Instant::now();
    for _ in 0..2 {
        seqlog.append(&entries).unwrap();
    }
    println!("{:?}", start.elapsed());
}
