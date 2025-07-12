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

// Local memcached backend using raw TCP
#[derive(Clone)]
pub struct LocalMemcachedBackend {
    servers: Vec<String>,
}

impl LocalMemcachedBackend {
    pub fn new(servers: Vec<String>) -> Self {
        Self { servers }
    }
    
    async fn send_command(&self, cmd: &str) -> Result<Vec<u8>, MomentoError> {
        use tokio::net::TcpStream;
        use tokio::io::{AsyncWriteExt, AsyncReadExt};
        
        let server = self.servers.first()
            .ok_or_else(|| MomentoError {
                message: "No servers configured".to_string(),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        let mut stream = TcpStream::connect(server).await
            .map_err(|e| MomentoError {
                message: format!("Failed to connect to memcached: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        stream.write_all(cmd.as_bytes()).await
            .map_err(|e| MomentoError {
                message: format!("Failed to send command: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        let mut response = Vec::new();
        let mut buf = [0u8; 8192];
        
        loop {
            let n = stream.read(&mut buf).await
                .map_err(|e| MomentoError {
                    message: format!("Failed to read response: {}", e),
                    error_code: momento::MomentoErrorCode::InternalServerError,
                    inner_error: None,
                    details: None,
                })?;
            
            if n == 0 {
                break;
            }
            
            response.extend_from_slice(&buf[..n]);
            
            // Check if we have a complete response
            if response.ends_with(b"\r\n") {
                if response.starts_with(b"VALUE") {
                    // For VALUE responses, check for END\r\n
                    if response.ends_with(b"END\r\n") {
                        break;
                    }
                } else {
                    // For other responses, single line is enough
                    break;
                }
            }
        }
        
        Ok(response)
    }
}

#[async_trait]
impl CacheBackend for LocalMemcachedBackend {
    async fn get(&self, _cache_name: &str, key: &[u8]) -> Result<GetResponse, MomentoError> {
        let key_str = String::from_utf8_lossy(key);
        let cmd = format!("get {}\r\n", key_str);
        
        let response = self.send_command(&cmd).await?;
        let response_str = String::from_utf8_lossy(&response);
        
        if response_str.starts_with("VALUE") {
            // Parse VALUE <key> <flags> <bytes>\r\n<data>\r\nEND\r\n
            let lines: Vec<&str> = response_str.lines().collect();
            if lines.len() >= 3 {
                let value_line = lines[0];
                let parts: Vec<&str> = value_line.split_whitespace().collect();
                if parts.len() >= 4 {
                    let flags = parts[2].parse::<u32>().unwrap_or(0);
                    let data_line = lines[1];
                    
                    // Prepend flags to data as 4 bytes (big-endian)
                    let mut value = flags.to_be_bytes().to_vec();
                    value.extend_from_slice(data_line.as_bytes());
                    
                    return Ok(GetResponse::Hit { value });
                }
            }
        }
        
        Ok(GetResponse::Miss)
    }
    
    async fn set(&self, _cache_name: &str, key: Vec<u8>, value: Vec<u8>, ttl: Option<Duration>) -> Result<(), MomentoError> {
        let key_str = String::from_utf8_lossy(&key);
        let expiration = ttl.map(|d| d.as_secs() as u32).unwrap_or(0);
        
        // Extract flags from first 4 bytes of value if present
        let (flags, data) = if value.len() >= 4 {
            let flags = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
            (flags, &value[4..])
        } else {
            (0, value.as_slice())
        };
        
        let cmd = format!("set {} {} {} {}\r\n", key_str, flags, expiration, data.len());
        
        use tokio::net::TcpStream;
        use tokio::io::{AsyncWriteExt, AsyncReadExt};
        
        let server = self.servers.first()
            .ok_or_else(|| MomentoError {
                message: "No servers configured".to_string(),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        let mut stream = TcpStream::connect(server).await
            .map_err(|e| MomentoError {
                message: format!("Failed to connect to memcached: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Send command header
        stream.write_all(cmd.as_bytes()).await
            .map_err(|e| MomentoError {
                message: format!("Failed to send command: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Send data
        stream.write_all(data).await
            .map_err(|e| MomentoError {
                message: format!("Failed to send data: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Send \r\n
        stream.write_all(b"\r\n").await
            .map_err(|e| MomentoError {
                message: format!("Failed to send terminator: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        // Read response
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await
            .map_err(|e| MomentoError {
                message: format!("Failed to read response: {}", e),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;
        
        let response = String::from_utf8_lossy(&buf[..n]);
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
    
    async fn delete(&self, _cache_name: &str, key: Vec<u8>) -> Result<(), MomentoError> {
        let key_str = String::from_utf8_lossy(&key);
        let cmd = format!("delete {}\r\n", key_str);
        
        let response = self.send_command(&cmd).await?;
        let response_str = String::from_utf8_lossy(&response);
        
        if !response_str.starts_with("DELETED") && !response_str.starts_with("NOT_FOUND") {
            return Err(MomentoError {
                message: format!("Delete failed: {}", response_str.trim()),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            });
        }
        
        Ok(())
    }
}