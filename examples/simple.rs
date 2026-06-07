use seqlog::SeqLog;

use std::time::Instant;

fn main() {
    let mut seqlog = SeqLog::open("target/example/run/").unwrap();

    seqlog.set_rotate(1024 * 128, 20);

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
        "0fffffffffffffffffffffffffffffffffffffffffffff",
    ];

    let start = Instant::now();
    for _ in 0..2 {
        seqlog.append(&entries).unwrap();
    }
    seqlog.sync().unwrap();
    println!("{:?}", start.elapsed());

    let mut count = 0;
    let mut scanner = seqlog.new_scanner(19234).unwrap();
    loop {
        let entry = scanner.next().unwrap();
        if entry.len() == 0 {
            println!("EOF");
            return;
        }

        count += 1;

        match std::str::from_utf8(entry) {
            Ok(s) => println!("{s}"),
            Err(err) => {
                println!("error: {err} {count} {}", entry.len());
                return;
            }
        }
    }
}
