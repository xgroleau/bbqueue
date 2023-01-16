use core::task::Waker;

/// A waker storage. Can be initialized without a waker, and a waker can be set on an eventual `poll` call.
/// The waker can be set and woken up.
#[derive(Debug)]
pub struct WakerStorage {
    waker: Option<Waker>,
}

impl WakerStorage {
    pub const fn new() -> Self {
        WakerStorage { waker: None }
    }

    /// Set the waker, will wake the previous one if one was already stored.
    pub fn set(&mut self, new: &Waker) {
        match &mut self.waker {
            // No need to clone if they wake the same task.
            Some(prev) if (prev.will_wake(new)) => {}
            // Replace and wake previous
            v => {
                if let Some(prev) = v.replace(new.clone()) {
                    prev.wake()
                }
            }
        }
    }

    /// Wake the waker if one is available
    pub fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake()
        }
    }
}
