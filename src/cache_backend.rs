use async_trait::async_trait;
use momento::cache::{GetResponse as MomentoGetResponse, CacheClient};
use momento::MomentoError;
use std::time::Duration;

// Wrapper enum to handle both Momento and local responses
#[derive(Debug)]
pub enum GetResponse {
    Hit { value: Vec<u8> },
    Miss,
}

impl From<MomentoGetResponse> for GetResponse {
    fn from(resp: MomentoGetResponse) -> Self {
        match resp {
            MomentoGetResponse::Hit { value } => GetResponse::Hit { value: value.into() },
            MomentoGetResponse::Miss => GetResponse::Miss,
        }
    }
}

// Custom error type for memcache operations
#[derive(Debug)]
struct MemcacheError(String);

impl std::fmt::Display for MemcacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Memcache error: {}", self.0)
    }
}

impl std::error::Error for MemcacheError {}

#[async_trait]
pub trait CacheBackend: Clone + Send + Sync + 'static {
    async fn get(&self, cache_name: &str, key: &[u8]) -> Result<GetResponse, MomentoError>;
    async fn set(&self, cache_name: &str, key: Vec<u8>, value: Vec<u8>, ttl: Option<Duration>) -> Result<(), MomentoError>;
    async fn delete(&self, cache_name: &str, key: Vec<u8>) -> Result<(), MomentoError>;
}

// Momento backend
#[derive(Clone)]
pub struct MomentoCacheBackend(pub CacheClient);

#[async_trait]
impl CacheBackend for MomentoCacheBackend {
    async fn get(&self, cache_name: &str, key: &[u8]) -> Result<GetResponse, MomentoError> {
        self.0.get(cache_name, key).await.map(|r| r.into())
    }
    
    async fn set(&self, cache_name: &str, key: Vec<u8>, value: Vec<u8>, ttl: Option<Duration>) -> Result<(), MomentoError> {
        use momento::cache::SetRequest;
        let request = SetRequest::new(cache_name, key, value).ttl(ttl);
        self.0.send_request(request).await?;
        Ok(())
    }
    
    async fn delete(&self, cache_name: &str, key: Vec<u8>) -> Result<(), MomentoError> {
        self.0.delete(cache_name, key).await?;
        Ok(())
    }
}

// Local memcached backend
#[derive(Clone)]
pub struct LocalMemcachedBackend {
    servers: Vec<String>,
}

impl LocalMemcachedBackend {
    pub fn new(servers: Vec<String>) -> Self {
        eprintln!("LocalMemcachedBackend configured with servers: {:?}", servers);
        Self { servers }
    }
    
    fn connect(&self) -> Result<memcache::Client, MomentoError> {
        memcache::connect(self.servers.clone())
            .map_err(|e| {
                eprintln!("Failed to connect to memcached servers {:?}: {}", self.servers, e);
                MomentoError {
                    message: format!("Failed to connect to memcached: {}", e),
                    error_code: momento::MomentoErrorCode::InternalServerError,
                    inner_error: None,
                    details: None,
                }
            })
    }
}

#[async_trait]
impl CacheBackend for LocalMemcachedBackend {
    async fn get(&self, _cache_name: &str, key: &[u8]) -> Result<GetResponse, MomentoError> {
        let client = self.connect()?;
        let key_str = String::from_utf8_lossy(key);
        
        // Get value from memcached (ignoring flags)
        match client.get::<Vec<u8>>(&key_str.as_ref()) {
            Ok(Some(data)) => {
                // Prepend 4 zero bytes for flags to match momento format
                let mut value = vec![0, 0, 0, 0];
                value.extend_from_slice(&data);
                Ok(GetResponse::Hit { value })
            },
            Ok(None) => Ok(GetResponse::Miss),
            Err(e) => Err(MomentoError {
                message: format!("Memcached get error: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            }),
        }
    }
    
    async fn set(&self, _cache_name: &str, key: Vec<u8>, value: Vec<u8>, ttl: Option<Duration>) -> Result<(), MomentoError> {
        let client = self.connect()?;
        let key_str = String::from_utf8_lossy(&key);
        let expiration = ttl.map(|d| d.as_secs() as u32).unwrap_or(0);
        
        // Skip the first 4 bytes (flags) if present
        let data = if value.len() >= 4 {
            &value[4..]
        } else {
            value.as_slice()
        };
        
        // Store just the data, ignoring flags
        client.set(&key_str.as_ref(), data, expiration)
            .map_err(|e| MomentoError {
                message: format!("Memcached set error: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        Ok(())
    }
    
    async fn delete(&self, _cache_name: &str, key: Vec<u8>) -> Result<(), MomentoError> {
        let client = self.connect()?;
        let key_str = String::from_utf8_lossy(&key);
        
        client.delete(&key_str.as_ref())
            .map_err(|e| MomentoError {
                message: format!("Memcached delete error: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        Ok(())
    }
}