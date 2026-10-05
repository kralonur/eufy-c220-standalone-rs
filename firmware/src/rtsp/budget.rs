//! Attach retained-memory reservations to the allocation itself. Clones and
//! slices retain the same permit until the final `Bytes` owner disappears.
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) struct MediaBudget(Arc<Semaphore>);

struct Payload {
    data: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl MediaBudget {
    pub fn new(bytes: u32) -> Self {
        Self(Arc::new(Semaphore::new(bytes as usize)))
    }

    fn reserve(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.0)
            .try_acquire_many_owned(u32::try_from(bytes).ok()?)
            .ok()
    }

    pub fn copy(&self, data: &[u8]) -> Option<Bytes> {
        let permit = self.reserve(data.len())?;
        Some(Bytes::from_owner(Payload {
            data: data.to_vec(),
            _permit: permit,
        }))
    }

    pub fn take(&self, data: Vec<u8>) -> Option<Bytes> {
        // Charge allocation capacity, not merely a short visible slice.
        let permit = self.reserve(data.capacity())?;
        Some(Bytes::from_owner(Payload {
            data,
            _permit: permit,
        }))
    }
}

// Shared audio slices and in-flight video fragments must never release a
// reservation early or retain an uncharged oversized Vec allocation.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_hold_the_entire_allocation_until_the_final_owner_leaves() {
        let budget = MediaBudget::new(8);
        let payload = budget.copy(&[0; 8]).unwrap();
        let slice = payload.slice(..1);
        drop(payload);
        assert!(budget.copy(&[0]).is_none());
        drop(slice);
        assert!(budget.copy(&[0; 8]).is_some());
        let mut oversized = Vec::with_capacity(9);
        oversized.push(0);
        assert!(budget.take(oversized).is_none());
    }
}
