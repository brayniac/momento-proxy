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

use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, AsyncReadExt, AsyncBufReadExt, BufReader};
use std::sync::Arc;
use tokio::sync::Semaphore;

// Local memcached backend using async TCP with connection pooling
#[derive(Clone)]
pub struct LocalMemcachedBackend {
    addr: String,
    connection_limit: Arc<Semaphore>,
}

impl LocalMemcachedBackend {
    pub fn new(servers: Vec<String>) -> Self {
        eprintln!("LocalMemcachedBackend configured with servers: {:?}", servers);
        
        let addr = servers.first()
            .expect("No servers configured")
            .strip_prefix("memcache://")
            .unwrap_or(servers.first().unwrap())
            .to_string();
        
        Self {
            addr,
            connection_limit: Arc::new(Semaphore::new(100)), // Allow up to 100 concurrent connections
        }
    }
    
    async fn execute_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, MomentoError> {
        let _permit = self.connection_limit.acquire().await.unwrap();
        
        let mut stream = TcpStream::connect(&self.addr).await
            .map_err(|e| MomentoError {
                message: format!("Failed to connect to memcached: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Set TCP_NODELAY for lower latency
        stream.set_nodelay(true).ok();
        
        // Send GET command
        let key_str = String::from_utf8_lossy(key);
        let cmd = format!("get {}\r\n", key_str);
        stream.write_all(cmd.as_bytes()).await
            .map_err(|e| MomentoError {
                message: format!("Failed to send GET command: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Read response
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).await
            .map_err(|e| MomentoError {
                message: format!("Failed to read response: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        if line.starts_with("VALUE") {
            // Parse VALUE <key> <flags> <bytes>\r\n
            let parts: Vec<&str> = line.trim().split_whitespace().collect();
            if parts.len() >= 4 {
                let size = parts[3].parse::<usize>().unwrap_or(0);
                let mut data = vec![0u8; size];
                reader.read_exact(&mut data).await
                    .map_err(|e| MomentoError {
                        message: format!("Failed to read data: {}", e),
                        error_code: momento::MomentoErrorCode::InternalServerError,
                        inner_error: None,
                        details: None,
                    })?;
                
                // Read \r\n after data
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf).await.ok();
                
                // Read END\r\n
                let mut end_line = String::new();
                reader.read_line(&mut end_line).await.ok();
                
                return Ok(Some(data));
            }
        }
        
        Ok(None)
    }
    
    async fn execute_set(&self, key: &[u8], data: &[u8], expiration: u32) -> Result<(), MomentoError> {
        let _permit = self.connection_limit.acquire().await.unwrap();
        
        let mut stream = TcpStream::connect(&self.addr).await
            .map_err(|e| MomentoError {
                message: format!("Failed to connect to memcached: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Set TCP_NODELAY for lower latency
        stream.set_nodelay(true).ok();
        
        // Send SET command: set <key> <flags> <exptime> <bytes>\r\n<data>\r\n
        let key_str = String::from_utf8_lossy(key);
        let cmd = format!("set {} 0 {} {}\r\n", key_str, expiration, data.len());
        
        // Write command, data, and CRLF in one go for efficiency
        let mut request = Vec::with_capacity(cmd.len() + data.len() + 2);
        request.extend_from_slice(cmd.as_bytes());
        request.extend_from_slice(data);
        request.extend_from_slice(b"\r\n");
        
        stream.write_all(&request).await
            .map_err(|e| MomentoError {
                message: format!("Failed to send SET command: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Read response
        let mut reader = BufReader::new(&mut stream);
        let mut response = String::new();
        reader.read_line(&mut response).await
            .map_err(|e| MomentoError {
                message: format!("Failed to read response: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        if !response.starts_with("STORED") {
            return Err(MomentoError {
                message: format!("Set failed: {}", response.trim()),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            });
        }
        
        Ok(())
    }
    
    async fn execute_delete(&self, key: &[u8]) -> Result<(), MomentoError> {
        let _permit = self.connection_limit.acquire().await.unwrap();
        
        let mut stream = TcpStream::connect(&self.addr).await
            .map_err(|e| MomentoError {
                message: format!("Failed to connect to memcached: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Set TCP_NODELAY for lower latency
        stream.set_nodelay(true).ok();
        
        // Send DELETE command
        let key_str = String::from_utf8_lossy(key);
        let cmd = format!("delete {}\r\n", key_str);
        stream.write_all(cmd.as_bytes()).await
            .map_err(|e| MomentoError {
                message: format!("Failed to send DELETE command: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Read response
        let mut reader = BufReader::new(&mut stream);
        let mut response = String::new();
        reader.read_line(&mut response).await
            .map_err(|e| MomentoError {
                message: format!("Failed to read response: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        if !response.starts_with("DELETED") && !response.starts_with("NOT_FOUND") {
            return Err(MomentoError {
                message: format!("Delete failed: {}", response.trim()),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            });
        }
        
        Ok(())
    }
}

#[async_trait]
impl CacheBackend for LocalMemcachedBackend {
    async fn get(&self, _cache_name: &str, key: &[u8]) -> Result<GetResponse, MomentoError> {
        match self.execute_get(key).await? {
            Some(data) => {
                // Prepend 4 zero bytes for flags to match momento format
                let mut value = vec![0, 0, 0, 0];
                value.extend_from_slice(&data);
                Ok(GetResponse::Hit { value })
            }
            None => Ok(GetResponse::Miss),
        }
    }
    
    async fn set(&self, _cache_name: &str, key: Vec<u8>, value: Vec<u8>, ttl: Option<Duration>) -> Result<(), MomentoError> {
        let expiration = ttl.map(|d| d.as_secs() as u32).unwrap_or(0);
        
        // Skip the first 4 bytes (flags) if present
        let data = if value.len() >= 4 {
            &value[4..]
        } else {
            value.as_slice()
        };
        
        self.execute_set(&key, data, expiration).await
    }
    
    async fn delete(&self, _cache_name: &str, key: Vec<u8>) -> Result<(), MomentoError> {
        self.execute_delete(&key).await
    }
}