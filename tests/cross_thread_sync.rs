use seqlog::{Result, SeqLogSyncer};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

mod common;

#[test]
fn cross_thread_sync() -> Result<()> {
    let mut store = common::open("target/tests-x-sync")?;

    let syncer = store.syncer()?;

    let (tx, rx) = sync_channel(1000);

    std::thread::spawn(|| sync_back_ground(rx, syncer));

    let next_seq0 = store.next_seq();
    let sync_seq0 = store.sync_seq();

    for _ in 0..10 {
        common::append10(&mut store)?;
        let _ = tx.send(());
    }
    std::thread::sleep(std::time::Duration::from_micros(1));
    for _ in 0..10 {
        common::append10(&mut store)?;
        let _ = tx.send(());
    }

    std::thread::sleep(std::time::Duration::from_millis(10));
    assert_eq!(store.next_seq(), next_seq0 + 200); // update
    assert_eq!(store.sync_seq(), sync_seq0 + 200); // update

    Ok(())
}

fn sync_back_ground(rx: Receiver<()>, mut syncer: SeqLogSyncer) {
    while let Ok(_) = rx.recv() {
        // consume all pendings
        let mut count = 1;
        loop {
            match rx.try_recv() {
                Ok(_) => count += 1,
                Err(TryRecvError::Empty) => break,
                Err(_) => return,
            }
        }

        // then sync
        let _ = syncer.sync();
        println!("--- sync {count}");
    }
}
