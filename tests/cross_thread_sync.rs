use seqlog::{Result, SeqLog, SeqLogSyncer};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

fn open(dir: &str) -> Result<SeqLog> {
    if !std::fs::exists(dir)? {
        SeqLog::create(dir, 100)
    } else {
        SeqLog::open(dir)
    }
}

#[test]
fn cross_thread_sync() -> Result<()> {
    let mut store = open("target/tests-store")?;

    let syncer = store.syncer()?;

    let (tx, rx) = sync_channel(1000);

    std::thread::spawn(|| sync_back_ground(rx, syncer));

    let next_seq0 = store.next_seq();
    let sync_seq0 = store.sync_seq();

    // append new data
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

    for _ in 0..10 {
        store.append(&entries)?;
        let _ = tx.send(());
    }
    std::thread::sleep(std::time::Duration::from_micros(1));
    for _ in 0..10 {
        store.append(&entries)?;
        let _ = tx.send(());
    }

    std::thread::sleep(std::time::Duration::from_millis(10));
    assert_eq!(store.next_seq(), next_seq0 + 200); // update
    assert_eq!(store.sync_seq(), sync_seq0 + 200); // update

    Ok(())
}

fn sync_back_ground(rx: Receiver<()>, mut syncer: SeqLogSyncer) {
    while let Ok(_) = rx.recv() {
        // receive all pendings
        let mut count = 1;
        loop {
            match rx.try_recv() {
                Ok(_) => count += 1,
                Err(TryRecvError::Empty) => break,
                Err(_) => return,
            }
        }

        let _ = syncer.sync();
        println!("--- sync {count}");
    }
}
