use seqlog::{Config, SeqLog};

fn main() {
    let mut seqlog = SeqLog::open("target/example/run/", Config::default()).unwrap();

    let entries = vec!["hello", "world"];
    seqlog.append(&entries).unwrap();
}
