//! NOTE: this crate is really just a shim for testing
//! the other no-std crate.

mod async_framed;
mod async_usage;
mod framed;
mod multi_thread;
mod ring_around_the_senders;
mod single_thread;

#[cfg(test)]
mod tests {
    use bbqueue::{BBQueue, Error as BBQError, StaticStorageProvider};

    #[test]
    fn deref_deref_mut() {
        let bb: BBQueue<StaticStorageProvider<6>> = BBQueue::new_static();
        let (mut prod, mut cons) = bb.try_split().unwrap();

        let mut wgr = prod.grant_exact(1).unwrap();

        // deref_mut
        wgr[0] = 123;

        assert_eq!(wgr.len(), 1);

        wgr.commit(1);

        // deref
        let rgr = cons.read().unwrap();

        assert_eq!(rgr[0], 123);

        rgr.release(1);
    }

    #[test]
    fn static_allocator() {
        // Check we can make multiple static items...
        static mut BBQ1: BBQueue<StaticStorageProvider<6>> = BBQueue::new_static();
        static mut BBQ2: BBQueue<StaticStorageProvider<6>> = BBQueue::new_static();
        let (mut prod1, mut cons1) = unsafe { BBQ1.try_split().unwrap() };
        let (mut _prod2, mut cons2) = unsafe { BBQ2.try_split().unwrap() };

        // ... and they aren't the same
        let mut wgr1 = prod1.grant_exact(3).unwrap();
        wgr1.copy_from_slice(&[1, 2, 3]);
        wgr1.commit(3);

        // no data here...
        assert!(cons2.read().is_err());

        // ...data is here!
        let rgr1 = cons1.read().unwrap();
        assert_eq!(&*rgr1, &[1, 2, 3]);
    }

    #[test]
    fn user_allocator() {
        // Check we can make multiple static items...
        let mut buf1 = [0; 6];
        let mut buf2 = [0; 6];
        let bqq1 = BBQueue::new_from_slice(&mut buf1);
        let bbq2 = BBQueue::new_from_slice(&mut buf2);
        let (mut prod1, mut cons1) = bqq1.try_split().unwrap();
        let (mut _prod2, mut cons2) = bbq2.try_split().unwrap();

        // ... and they aren't the same
        let mut wgr1 = prod1.grant_exact(3).unwrap();
        wgr1.copy_from_slice(&[1, 2, 3]);
        wgr1.commit(3);

        // no data here...
        assert!(cons2.read().is_err());

        // ...data is here!
        let rgr1 = cons1.read().unwrap();
        assert_eq!(&*rgr1, &[1, 2, 3]);
    }

    #[test]
    fn release() {
        // Check we can make multiple static items...
        static mut BBQ1: BBQueue<StaticStorageProvider<6>> = BBQueue::new_static();
        static mut BBQ2: BBQueue<StaticStorageProvider<6>> = BBQueue::new_static();
        unsafe {
            let (prod1, cons1) = BBQ1.try_split().unwrap();
            let (prod2, cons2) = BBQ2.try_split().unwrap();

            // We cannot release with the wrong prod/cons
            let (prod2, cons2) = BBQ1.try_release(prod2, cons2).unwrap_err();
            let (prod1, cons1) = BBQ2.try_release(prod1, cons1).unwrap_err();

            // We cannot release with the wrong consumer...
            let (prod1, cons2) = BBQ1.try_release(prod1, cons2).unwrap_err();

            // ...or the wrong producer
            let (prod2, cons1) = BBQ1.try_release(prod2, cons1).unwrap_err();

            // We cannot release with a write grant in progress
            let mut prod1 = prod1;
            let wgr1 = prod1.grant_exact(3).unwrap();
            let (prod1, mut cons1) = BBQ1.try_release(prod1, cons1).unwrap_err();

            // We cannot release with a read grant in progress
            wgr1.commit(3);
            let rgr1 = cons1.read().unwrap();
            let (prod1, cons1) = BBQ1.try_release(prod1, cons1).unwrap_err();

            // But we can when everything is resolved
            rgr1.release(3);
            assert!(BBQ1.try_release(prod1, cons1).is_ok());
            assert!(BBQ2.try_release(prod2, cons2).is_ok());

            // And we can re-split on-demand
            let _ = BBQ1.try_split().unwrap();
            let _ = BBQ2.try_split().unwrap();
        }
    }

    #[test]
    fn direct_usage_sanity() {
        // Initialize
        let bb: BBQueue<StaticStorageProvider<6>> = BBQueue::new_static();
        let (mut prod, mut cons) = bb.try_split().unwrap();
        assert_eq!(cons.read(), Err(BBQError::InsufficientSize));

        // Initial grant, shouldn't roll over
        let mut x = prod.grant_exact(4).unwrap();

        // Still no data available yet
        assert_eq!(cons.read(), Err(BBQError::InsufficientSize));

        // Add full data from grant
        x.copy_from_slice(&[1, 2, 3, 4]);

        // Still no data available yet
        assert_eq!(cons.read(), Err(BBQError::InsufficientSize));

        // Commit data
        x.commit(4);

        ::std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

        let a = cons.read().unwrap();
        assert_eq!(&*a, &[1, 2, 3, 4]);

        // Release the first two bytes
        a.release(2);

        let r = cons.read().unwrap();
        assert_eq!(&*r, &[3, 4]);
        r.release(0);

        // Grant two more
        let mut x = prod.grant_exact(2).unwrap();
        let r = cons.read().unwrap();
        assert_eq!(&*r, &[3, 4]);
        r.release(0);

        // Add more data
        x.copy_from_slice(&[11, 12]);
        let r = cons.read().unwrap();
        assert_eq!(&*r, &[3, 4]);
        r.release(0);

        // Commit
        x.commit(2);

        let a = cons.read().unwrap();
        assert_eq!(&*a, &[3, 4, 11, 12]);

        a.release(2);
        let r = cons.read().unwrap();
        assert_eq!(&*r, &[11, 12]);
        r.release(0);

        let mut x = prod.grant_exact(3).unwrap();
        let r = cons.read().unwrap();
        assert_eq!(&*r, &[11, 12]);
        r.release(0);

        x.copy_from_slice(&[21, 22, 23]);

        let r = cons.read().unwrap();
        assert_eq!(&*r, &[11, 12]);
        r.release(0);
        x.commit(3);

        let a = cons.read().unwrap();

        // NOTE: The data we just added isn't available yet,
        // since it has wrapped around
        assert_eq!(&*a, &[11, 12]);

        a.release(2);

        // And now we can see it
        let r = cons.read().unwrap();
        assert_eq!(&*r, &[21, 22, 23]);
        r.release(0);

        // Ask for something way too big
        assert!(prod.grant_exact(10).is_err());
    }

    #[test]
    fn zero_sized_grant() {
        let bb: BBQueue<StaticStorageProvider<1000>> = BBQueue::new_static();
        let (mut prod, mut _cons) = bb.try_split().unwrap();

        let size = 1000;
        let grant = prod.grant_exact(size).unwrap();
        grant.commit(size);

        let grant = prod.grant_exact(0).unwrap();
        grant.commit(0);
    }

    #[test]
    fn frame_sanity() {
        let bb: BBQueue<StaticStorageProvider<1000>> = BBQueue::new_static();
        let (mut prod, mut cons) = bb.try_split_framed().unwrap();

        // One frame in, one frame out
        let mut wgrant = prod.grant(128).unwrap();
        assert_eq!(wgrant.len(), 128);
        for (idx, i) in wgrant.iter_mut().enumerate() {
            *i = idx as u8;
        }
        wgrant.commit(128);

        let rgrant = cons.read().unwrap();
        assert_eq!(rgrant.len(), 128);
        for (idx, i) in rgrant.iter().enumerate() {
            assert_eq!(*i, idx as u8);
        }
        rgrant.release();

        // Three frames in, three frames out
        let mut state = 0;
        let states = [16usize, 32, 24];

        for step in &states {
            let mut wgrant = prod.grant(*step).unwrap();
            assert_eq!(wgrant.len(), *step);
            for (idx, i) in wgrant.iter_mut().enumerate() {
                *i = (idx + state) as u8;
            }
            wgrant.commit(*step);
            state += *step;
        }

        state = 0;

        for step in &states {
            let rgrant = cons.read().unwrap();
            assert_eq!(rgrant.len(), *step);
            for (idx, i) in rgrant.iter().enumerate() {
                assert_eq!(*i, (idx + state) as u8);
            }
            rgrant.release();
            state += *step;
        }
    }

    #[test]
    fn frame_wrap() {
        let bb: BBQueue<StaticStorageProvider<22>> = BBQueue::new_static();
        let (mut prod, mut cons) = bb.try_split_framed().unwrap();

        // 10 + 1 used
        let mut wgrant = prod.grant(10).unwrap();
        assert_eq!(wgrant.len(), 10);
        for (idx, i) in wgrant.iter_mut().enumerate() {
            *i = idx as u8;
        }
        wgrant.commit(10);
        // 1 frame in queue

        // 20 + 2 used (assuming u64 test platform)
        let mut wgrant = prod.grant(10).unwrap();
        assert_eq!(wgrant.len(), 10);
        for (idx, i) in wgrant.iter_mut().enumerate() {
            *i = idx as u8;
        }
        wgrant.commit(10);
        // 2 frames in queue

        let rgrant = cons.read().unwrap();
        assert_eq!(rgrant.len(), 10);
        for (idx, i) in rgrant.iter().enumerate() {
            assert_eq!(*i, idx as u8);
        }
        rgrant.release();
        // 1 frame in queue

        // No more room!
        assert!(prod.grant(10).is_err());

        let rgrant = cons.read().unwrap();
        assert_eq!(rgrant.len(), 10);
        for (idx, i) in rgrant.iter().enumerate() {
            assert_eq!(*i, idx as u8);
        }
        rgrant.release();
        // 0 frames in queue

        // 10 + 1 used (assuming u64 test platform)
        let mut wgrant = prod.grant(10).unwrap();
        assert_eq!(wgrant.len(), 10);
        for (idx, i) in wgrant.iter_mut().enumerate() {
            *i = idx as u8;
        }
        wgrant.commit(10);
        // 1 frame in queue

        // No more room!
        assert!(prod.grant(10).is_err());

        let rgrant = cons.read().unwrap();
        assert_eq!(rgrant.len(), 10);
        for (idx, i) in rgrant.iter().enumerate() {
            assert_eq!(*i, idx as u8);
        }
        rgrant.release();
        // 0 frames in queue

        // No more frames!
        assert!(cons.read().is_none());
    }

    #[test]
    fn frame_big_little() {
        let bb: BBQueue<StaticStorageProvider<65536>> = BBQueue::new_static();
        let (mut prod, mut cons) = bb.try_split_framed().unwrap();

        // Create a frame that should take 3 bytes for the header
        assert!(prod.grant(65534).is_err());

        let mut wgrant = prod.grant(65533).unwrap();
        assert_eq!(wgrant.len(), 65533);
        for (idx, i) in wgrant.iter_mut().enumerate() {
            *i = idx as u8;
        }
        // Only commit 127 bytes, which fit into a header of 1 byte
        wgrant.commit(127);

        let rgrant = cons.read().unwrap();
        assert_eq!(rgrant.len(), 127);
        for (idx, i) in rgrant.iter().enumerate() {
            assert_eq!(*i, idx as u8);
        }
        rgrant.release();
    }

    #[test]
    fn split_sanity_check() {
        let bb: BBQueue<StaticStorageProvider<10>> = BBQueue::new_static();
        let (mut prod, mut cons) = bb.try_split().unwrap();

        // Fill buffer
        let mut wgrant = prod.grant_exact(10).unwrap();
        assert_eq!(wgrant.len(), 10);
        wgrant.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        wgrant.commit(10);

        let rgrant = cons.split_read().unwrap();
        assert_eq!(rgrant.combined_len(), 10);
        assert_eq!(
            rgrant.bufs(),
            (&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10][..], &[][..])
        );
        // Release part of the buffer
        rgrant.release(6);

        // Almost fill buffer again => | 11 | 12 | 13 | 14 | 15 | x | 7 | 8 | 9 | 10 |
        let mut wgrant = prod.grant_exact(5).unwrap();
        assert_eq!(wgrant.len(), 5);
        wgrant.copy_from_slice(&[11, 12, 13, 14, 15]);
        wgrant.commit(5);

        let rgrant = cons.split_read().unwrap();
        assert_eq!(rgrant.combined_len(), 9);
        assert_eq!(
            rgrant.bufs(),
            (&[7, 8, 9, 10][..], &[11, 12, 13, 14, 15][..])
        );

        // Release part of the buffer => | x | x | x | 14 | 15 | x | x | x | x | x |
        rgrant.release(7);

        // Check that it is not possible to claim more space than what should be available
        assert!(prod.grant_exact(6).is_err());

        // Fill buffer to the end => | x | x | x | 14 | 15 | 21 | 22 | 23 | 24 | 25 |
        let mut wgrant = prod.grant_exact(5).unwrap();
        wgrant.copy_from_slice(&[21, 22, 23, 24, 25]);
        wgrant.commit(5);

        let rgrant = cons.split_read().unwrap();
        assert_eq!(rgrant.combined_len(), 7);
        assert_eq!(rgrant.bufs(), (&[14, 15, 21, 22, 23, 24, 25][..], &[][..]));
        rgrant.release(0);

        // Fill buffer to the end => | 26 | 27 | x | 14 | 15 | 21 | 22 | 23 | 24 | 25 |
        let mut wgrant = prod.grant_exact(2).unwrap();
        wgrant.copy_from_slice(&[26, 27]);
        wgrant.commit(2);

        // Fill buffer to the end => | x | 27 | x | x | x | x | x | x | x | x |
        let rgrant = cons.split_read().unwrap();
        assert_eq!(rgrant.combined_len(), 9);
        assert_eq!(
            rgrant.bufs(),
            (&[14, 15, 21, 22, 23, 24, 25][..], &[26, 27][..])
        );
        rgrant.release(8);

        let rgrant = cons.split_read().unwrap();
        assert_eq!(rgrant.combined_len(), 1);
        assert_eq!(rgrant.bufs(), (&[27][..], &[][..]));
        rgrant.release(1);
    }

    #[test]
    fn split_read_sanity_check() {
        let bb: BBQueue<StaticStorageProvider<6>> = BBQueue::new_static();
        let (mut prod, mut cons) = bb.try_split().unwrap();

        const ITERS: usize = 100000;

        for i in 0..ITERS {
            let j = (i & 255) as u8;

            #[cfg(feature = "extra-verbose")]
            println!("===========================");
            #[cfg(feature = "extra-verbose")]
            println!("INDEX: {:?}", j);
            #[cfg(feature = "extra-verbose")]
            println!("===========================");

            #[cfg(feature = "extra-verbose")]
            println!("START: {:?}", bb);

            let mut wgr = prod.grant_exact(1).unwrap();

            #[cfg(feature = "extra-verbose")]
            println!("GRANT: {:?}", bb);

            wgr[0] = j;

            #[cfg(feature = "extra-verbose")]
            println!("WRITE: {:?}", bb);

            wgr.commit(1);

            #[cfg(feature = "extra-verbose")]
            println!("COMIT: {:?}", bb);

            // This panicked before with Err(GrantInProgress), because SplitGrantR did not implement Drop
            let rgr = cons.split_read().unwrap();
            drop(rgr);

            #[cfg(feature = "extra-verbose")]
            println!("READ : {:?}", bb);

            let rgr = cons.split_read().unwrap();
            let (first, second) = rgr.bufs();
            if first.len() == 1 {
                assert_eq!(first[0], j);
            } else if second.len() == 1 {
                assert_eq!(second[0], j);
            } else {
                assert!(false, "wrong len");
            }

            #[cfg(feature = "extra-verbose")]
            println!("RELSE: {:?}", bb);

            rgr.release(1);

            #[cfg(feature = "extra-verbose")]
            println!("FINSH: {:?}", bb);
        }
    }
}
