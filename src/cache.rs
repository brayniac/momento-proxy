use std::{
    mem::size_of,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub enum LocalCache {
    SyncMoka(sync_moka::SyncMokaCache),
    AsyncMoka(async_moka::AsyncMokaCache),
    Foyer(foyer_cache::FoyerCache),
}

impl LocalCache {
    pub async fn get(&self, key: &[u8]) -> Option<CacheEntry> {
        match self {
            LocalCache::SyncMoka(cache) => cache.get(key),
            LocalCache::AsyncMoka(cache) => cache.get(key).await,
            LocalCache::Foyer(cache) => cache.get(key).await,
        }
    }

    pub async fn set(&self, key: Vec<u8>, value: impl Into<CacheValue>) {
        match self {
            LocalCache::SyncMoka(cache) => cache.set(key, value),
            LocalCache::AsyncMoka(cache) => cache.set(key, value).await,
            LocalCache::Foyer(cache) => cache.set(key, value).await,
        }
    }

    pub async fn delete(&self, key: &[u8]) -> Option<CacheValue> {
        match self {
            LocalCache::SyncMoka(cache) => cache.delete(key),
            LocalCache::AsyncMoka(cache) => cache.delete(key).await,
            LocalCache::Foyer(cache) => cache.delete(key).await,
        }
    }
}

use moka::Expiry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheValue {
    Memcached { value: protocol_memcache::Value },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    value: CacheValue,
    expire_at: Instant,
}

impl CacheEntry {
    pub fn _expiry_epoch_seconds(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(n) => {
                n.as_secs() as i64 + self.expire_at.duration_since(Instant::now()).as_secs() as i64
            }
            Err(_) => 0,
        }
    }

    pub fn into_value(self) -> CacheValue {
        self.value
    }
}

pub mod sync_moka {
    use super::*;
    use moka::sync::Cache;

    #[derive(Clone)]
    pub struct SyncMokaCache {
        cache: Cache<KeyType, CacheEntry>,
        ttl: Duration,
    }

    impl SyncMokaCache {
        pub fn new(max_bytes: usize, ttl: Duration) -> Self {
            let cache = Cache::builder()
                .max_capacity(max_bytes as u64)
                .weigher(super::weigh)
                .expire_after(super::MCacheExpiry)
                .build();
            Self {
                cache,
                ttl: std::cmp::min(ttl, Duration::from_secs(5 * 365 * 24 * 3600)),
            }
        }

        pub fn get(&self, key: &[u8]) -> Option<CacheEntry> {
            self.cache.get(key)
        }

        pub fn set(&self, key: Vec<u8>, value: impl Into<CacheValue>) {
            self.cache.insert(
                key,
                CacheEntry {
                    value: value.into(),
                    expire_at: Instant::now() + self.ttl,
                },
            )
        }

        pub fn delete(&self, key: &[u8]) -> Option<CacheValue> {
            self.cache.remove(key).map(|e| e.value)
        }
    }
}

pub mod async_moka {
    use super::*;
    use moka::future::Cache;

    #[derive(Clone)]
    pub struct AsyncMokaCache {
        cache: Cache<KeyType, CacheEntry>,
        ttl: Duration,
    }

    impl AsyncMokaCache {
        pub fn new(max_bytes: usize, ttl: Duration) -> Self {
            let cache = Cache::builder()
                .max_capacity(max_bytes as u64)
                .weigher(super::weigh)
                .expire_after(super::MCacheExpiry)
                .build();
            Self {
                cache,
                ttl: std::cmp::min(ttl, Duration::from_secs(5 * 365 * 24 * 3600)),
            }
        }

        pub async fn get(&self, key: &[u8]) -> Option<CacheEntry> {
            self.cache.get(key).await
        }

        pub async fn set(&self, key: Vec<u8>, value: impl Into<CacheValue>) {
            self.cache
                .insert(
                    key,
                    CacheEntry {
                        value: value.into(),
                        expire_at: Instant::now() + self.ttl,
                    },
                )
                .await
        }

        pub async fn delete(&self, key: &[u8]) -> Option<CacheValue> {
            self.cache.remove(key).await.map(|e| e.value)
        }
    }
}

pub mod foyer_cache {
    use super::*;
    use foyer::{DirectFsDeviceOptions, HybridCache, HybridCacheBuilder};
    use serde::{Deserialize, Serialize};

    // Wrapper types that implement serde for serialization
    #[derive(Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
    struct SerdeKey(Vec<u8>);

    #[derive(Clone, Serialize, Deserialize)]
    struct SerdeEntry {
        // Store the raw memcache value components
        key: Vec<u8>,
        data: Vec<u8>,
        flags: u32,
        expire_at_secs: i64, // Seconds since UNIX epoch
    }

    enum CacheType {
        Memory(foyer::Cache<SerdeKey, SerdeEntry>),
        Hybrid(HybridCache<SerdeKey, SerdeEntry>),
    }

    #[derive(Clone)]
    pub struct FoyerCache {
        cache: std::sync::Arc<CacheType>,
        ttl: Duration,
    }

    impl FoyerCache {
        pub async fn new(
            memory_bytes: usize,
            ttl: Duration,
            disk_bytes: usize,
            disk_dir: Option<&str>,
        ) -> Self {
            let ttl = std::cmp::min(ttl, Duration::from_secs(5 * 365 * 24 * 3600));

            let cache = if disk_bytes > 0 && disk_dir.is_some() {
                // Create hybrid cache with both memory and disk
                let hybrid = HybridCacheBuilder::new()
                    .memory(memory_bytes)
                    .with_weighter(|k: &SerdeKey, v: &SerdeEntry| {
                        k.0.len() + v.key.len() + v.data.len() + 32 // overhead
                    })
                    .storage(foyer::Engine::Large(Default::default())) // Use large object engine for disk storage
                    .with_device_options(
                        DirectFsDeviceOptions::new(disk_dir.unwrap())
                            .with_capacity(disk_bytes)
                            .with_file_size(4 * 1024 * 1024), // 4MB files
                    )
                    .build()
                    .await
                    .expect("Failed to build foyer hybrid cache");
                CacheType::Hybrid(hybrid)
            } else {
                // Create memory-only cache
                let mem_cache = foyer::CacheBuilder::new(memory_bytes)
                    .with_weighter(|k: &SerdeKey, v: &SerdeEntry| {
                        k.0.len() + v.key.len() + v.data.len() + 32
                    })
                    .build();
                CacheType::Memory(mem_cache)
            };

            Self {
                cache: std::sync::Arc::new(cache),
                ttl,
            }
        }

        pub async fn get(&self, key: &[u8]) -> Option<CacheEntry> {
            let serde_key = SerdeKey(key.to_vec());

            let entry_opt = match self.cache.as_ref() {
                CacheType::Memory(cache) => cache.get(&serde_key).map(|e| e.value().clone()),
                CacheType::Hybrid(cache) => match cache.get(&serde_key).await {
                    Ok(Some(entry)) => Some(entry.value().clone()),
                    _ => None,
                },
            };

            entry_opt.and_then(|serde_entry| {
                // Check if entry is expired
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let remaining_secs =
                    serde_entry.expire_at_secs.saturating_sub(now_secs).max(0) as u64;

                if remaining_secs > 0 {
                    let expire_at = Instant::now() + Duration::from_secs(remaining_secs);
                    Some(CacheEntry {
                        value: CacheValue::Memcached {
                            value: protocol_memcache::Value::new(
                                &serde_entry.key,
                                serde_entry.flags,
                                None,
                                &serde_entry.data,
                            ),
                        },
                        expire_at,
                    })
                } else {
                    None
                }
            })
        }

        pub async fn set(&self, key: Vec<u8>, value: impl Into<CacheValue>) {
            let value = value.into();
            let expire_at_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64 + self.ttl.as_secs() as i64)
                .unwrap_or(0);

            let serde_entry = match &value {
                CacheValue::Memcached {
                    value: memcache_value,
                } => {
                    // Extract the components we can access
                    SerdeEntry {
                        key: memcache_value.key().to_vec(),
                        data: memcache_value.value().unwrap_or_default().to_vec(),
                        flags: 0, // We can't access the flags from the public API
                        expire_at_secs,
                    }
                }
            };

            let serde_key = SerdeKey(key);
            match self.cache.as_ref() {
                CacheType::Memory(cache) => {
                    cache.insert(serde_key, serde_entry);
                }
                CacheType::Hybrid(cache) => {
                    cache.insert(serde_key, serde_entry);
                }
            }
        }

        pub async fn delete(&self, key: &[u8]) -> Option<CacheValue> {
            let serde_key = SerdeKey(key.to_vec());

            match self.cache.as_ref() {
                CacheType::Memory(cache) => cache.remove(&serde_key).map(|entry| {
                    let v = entry.value();
                    CacheValue::Memcached {
                        value: protocol_memcache::Value::new(&v.key, v.flags, None, &v.data),
                    }
                }),
                CacheType::Hybrid(cache) => {
                    // HybridCache remove doesn't return the value
                    cache.remove(&serde_key);
                    None
                }
            }
        }
    }
}

fn weigh(key: &KeyType, value: &CacheEntry) -> u32 {
    (key.len()
        + match &value.value {
            CacheValue::Memcached { value } => value.len().unwrap_or_default(),
        }
        + size_of::<protocol_memcache::Value>()) as u32
}

struct MCacheExpiry;

type KeyType = Vec<u8>;

impl Expiry<KeyType, CacheEntry> for MCacheExpiry {
    /// Returns the duration of the expiration of the value that was just
    /// created.
    fn expire_after_create(
        &self,
        _key: &KeyType,
        value: &CacheEntry,
        current_time: Instant,
    ) -> Option<Duration> {
        Some(value.expire_at.saturating_duration_since(current_time))
    }

    fn expire_after_update(
        &self,
        _key: &KeyType,
        value: &CacheEntry,
        updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(value.expire_at.saturating_duration_since(updated_at))
    }
}

use crate::momento_proxy::MemoryCacheImpl;

pub async fn create_cache(
    impl_type: MemoryCacheImpl,
    memory_bytes: usize,
    ttl: Duration,
    disk_bytes: usize,
    disk_dir: Option<&str>,
) -> LocalCache {
    match impl_type {
        MemoryCacheImpl::Moka => {
            LocalCache::SyncMoka(sync_moka::SyncMokaCache::new(memory_bytes, ttl))
        }
        MemoryCacheImpl::MokaAsync => {
            LocalCache::AsyncMoka(async_moka::AsyncMokaCache::new(memory_bytes, ttl))
        }
        MemoryCacheImpl::Foyer => LocalCache::Foyer(
            foyer_cache::FoyerCache::new(memory_bytes, ttl, disk_bytes, disk_dir).await,
        ),
    }
}
