// Copyright 2019-2022 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use cid::Cid;
use lru::LruCache;
use std::num::NonZeroUsize;
use tokio::sync::RwLock;

/// Thread-safe cache for tracking bad blocks.
/// This cache is checked before validating a block, to ensure no duplicate work.
#[derive(Debug)]
pub struct BadBlockCache {
    cache: RwLock<LruCache<Cid, String>>,
}

impl Default for BadBlockCache {
    fn default() -> Self {
        Self::new(forest_utils::const_option!(NonZeroUsize::new(1 << 15)))
    }
}

impl BadBlockCache {
    pub fn new(cap: NonZeroUsize) -> Self {
        Self {
            cache: RwLock::new(LruCache::new(cap)),
        }
    }

    /// Puts a bad block `Cid` in the cache with a given reason.
    pub async fn put(&self, c: Cid, reason: String) -> Option<String> {
        self.cache.write().await.put(c, reason)
    }

    /// Returns `Some` with the reason if the block CID is in bad block cache.
    /// This also updates the key to the head of the cache.
    pub async fn get(&self, c: &Cid) -> Option<String> {
        self.cache.write().await.get(c).cloned()
    }

    /// Returns `Some` with the reason if the block CID is in bad block cache.
    /// This function does not update the head position of the `Cid` key.
    pub async fn peek(&self, c: &Cid) -> Option<String> {
        self.cache.read().await.peek(c).cloned()
    }
}
