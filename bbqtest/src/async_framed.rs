#[cfg(test)]
mod tests {

    use bbqueue::{BBQueue, StaticBufferProvider};
    use futures::executor::block_on;

    #[test]
    fn frame_wrong_size() {
        block_on(async {
            let bb: BBQueue<StaticBufferProvider<256>> = BBQueue::new_static();
            let (mut prod, mut cons) = bb.try_split_framed().unwrap();

            // Create largeish grants
            let mut wgr = prod.grant_async(127).await.unwrap();
            for (i, by) in wgr.iter_mut().enumerate() {
                *by = i as u8;
            }
            // Note: In debug mode, this hits a debug_assert
            wgr.commit(256);

            let rgr = cons.read_async().await.unwrap();
            assert_eq!(rgr.len(), 127);
            for (i, by) in rgr.iter().enumerate() {
                assert_eq!((i as u8), *by);
            }
            rgr.release();
        });
    }

    #[test]
    fn full_size() {
        block_on(async {
            let bb: BBQueue<StaticBufferProvider<256>> = BBQueue::new_static();
            let (mut prod, mut cons) = bb.try_split_framed().unwrap();
            let mut ctr = 0;

            for _ in 0..10_000 {
                // Create largeish grants
                if let Ok(mut wgr) = prod.grant_async(127).await {
                    ctr += 1;
                    for (i, by) in wgr.iter_mut().enumerate() {
                        *by = i as u8;
                    }
                    wgr.commit(127);

                    let rgr = cons.read_async().await.unwrap();
                    assert_eq!(rgr.len(), 127);
                    for (i, by) in rgr.iter().enumerate() {
                        assert_eq!((i as u8), *by);
                    }
                    rgr.release();
                } else {
                    // Create smallish grants
                    let mut wgr = prod.grant_async(1).await.unwrap();
                    for (i, by) in wgr.iter_mut().enumerate() {
                        *by = i as u8;
                    }
                    wgr.commit(1);

                    let rgr = cons.read_async().await.unwrap();
                    assert_eq!(rgr.len(), 1);
                    for (i, by) in rgr.iter().enumerate() {
                        assert_eq!((i as u8), *by);
                    }
                    rgr.release();
                };
            }

            assert!(ctr > 1);
        });
    }

    #[test]
    fn frame_overcommit() {
        block_on(async {
            let bb: BBQueue<StaticBufferProvider<256>> = BBQueue::new_static();
            let (mut prod, mut cons) = bb.try_split_framed().unwrap();

            // Create largeish grants
            let mut wgr = prod.grant_async(128).await.unwrap();
            for (i, by) in wgr.iter_mut().enumerate() {
                *by = i as u8;
            }
            wgr.commit(255);

            let mut wgr = prod.grant_async(64).await.unwrap();
            for (i, by) in wgr.iter_mut().enumerate() {
                *by = (i as u8) + 128;
            }
            wgr.commit(127);

            let rgr = cons.read_async().await.unwrap();
            assert_eq!(rgr.len(), 128);
            rgr.release();

            let rgr = cons.read_async().await.unwrap();
            assert_eq!(rgr.len(), 64);
            rgr.release();
        });
    }

    #[test]
    fn frame_undercommit() {
        block_on(async {
            let bb: BBQueue<StaticBufferProvider<512>> = BBQueue::new_static();
            let (mut prod, mut cons) = bb.try_split_framed().unwrap();

            for _ in 0..100 {
                // Create largeish grants
                let mut wgr = prod.grant_async(128).await.unwrap();
                for (i, by) in wgr.iter_mut().enumerate() {
                    *by = i as u8;
                }
                wgr.commit(13);

                let mut wgr = prod.grant_async(64).await.unwrap();
                for (i, by) in wgr.iter_mut().enumerate() {
                    *by = (i as u8) + 128;
                }
                wgr.commit(7);

                let mut wgr = prod.grant_async(32).await.unwrap();
                for (i, by) in wgr.iter_mut().enumerate() {
                    *by = (i as u8) + 192;
                }
                wgr.commit(0);

                let rgr = cons.read_async().await.unwrap();
                assert_eq!(rgr.len(), 13);
                rgr.release();

                let rgr = cons.read_async().await.unwrap();
                assert_eq!(rgr.len(), 7);
                rgr.release();

                let rgr = cons.read_async().await.unwrap();
                assert_eq!(rgr.len(), 0);
                rgr.release();
            }
        });
    }

    #[test]
    fn frame_auto_commit_release() {
        block_on(async {
            let bb: BBQueue<StaticBufferProvider<256>> = BBQueue::new_static();
            let (mut prod, mut cons) = bb.try_split_framed().unwrap();

            for _ in 0..100 {
                {
                    let mut wgr = prod.grant_async(64).await.unwrap();
                    wgr.to_commit(64);
                    for (i, by) in wgr.iter_mut().enumerate() {
                        *by = i as u8;
                    }
                    // drop
                }

                {
                    let mut rgr = cons.read_async().await.unwrap();
                    rgr.auto_release(true);
                    let rgr = rgr;

                    for (i, by) in rgr.iter().enumerate() {
                        assert_eq!(*by, i as u8);
                    }
                    assert_eq!(rgr.len(), 64);
                    // drop
                }
            }

            assert!(cons.read().is_none());
        });
    }
}
