use async_trait::async_trait;
use momento::cache::{CacheClient, GetResponse as MomentoGetResponse};
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
            MomentoGetResponse::Hit { value } => GetResponse::Hit {
                value: value.into(),
            },
            MomentoGetResponse::Miss => GetResponse::Miss,
        }
    }
}


#[async_trait]
pub trait CacheBackend: Clone + Send + Sync + 'static {
    async fn get(&self, cache_name: &str, key: &[u8]) -> Result<GetResponse, MomentoError>;
    async fn set(
        &self,
        cache_name: &str,
        key: Vec<u8>,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> Result<(), MomentoError>;
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

    async fn set(
        &self,
        cache_name: &str,
        key: Vec<u8>,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> Result<(), MomentoError> {
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

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

// Commands to send to worker tasks
#[derive(Debug)]
enum Command {
    Get {
        key: Vec<u8>,
        response: oneshot::Sender<Result<Option<Vec<u8>>, MomentoError>>,
    },
    Set {
        key: Vec<u8>,
        data: Vec<u8>,
        expiration: u32,
        response: oneshot::Sender<Result<(), MomentoError>>,
    },
    Delete {
        key: Vec<u8>,
        response: oneshot::Sender<Result<(), MomentoError>>,
    },
}

// Worker task that maintains a persistent connection
async fn memcached_worker(addr: String, mut receiver: mpsc::Receiver<Command>) {
    loop {
        // Connect to memcached
        let stream = match TcpStream::connect(&addr).await {
            Ok(s) => {
                s.set_nodelay(true).ok();
                s
            }
            Err(e) => {
                eprintln!("Worker failed to connect to {}: {}", addr, e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = writer;

        // Process commands on this connection
        while let Some(cmd) = receiver.recv().await {
            match cmd {
                Command::Get { key, response } => {
                    let result = execute_get(&mut reader, &mut writer, &key).await;
                    let _ = response.send(result);
                }
                Command::Set {
                    key,
                    data,
                    expiration,
                    response,
                } => {
                    let result =
                        execute_set(&mut reader, &mut writer, &key, &data, expiration).await;
                    let _ = response.send(result);
                }
                Command::Delete { key, response } => {
                    let result = execute_delete(&mut reader, &mut writer, &key).await;
                    let _ = response.send(result);
                }
            }
        }
    }
}

async fn execute_get(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    key: &[u8],
) -> Result<Option<Vec<u8>>, MomentoError> {
    // Send GET command
    let key_str = String::from_utf8_lossy(key);
    let cmd = format!("get {}\r\n", key_str);
    writer
        .write_all(cmd.as_bytes())
        .await
        .map_err(|e| MomentoError {
            message: format!("Failed to send GET command: {}", e),
            error_code: momento::MomentoErrorCode::InternalServerError,
            inner_error: None,
            details: None,
        })?;

    // Read response
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
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
            reader
                .read_exact(&mut data)
                .await
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

async fn execute_set(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    key: &[u8],
    data: &[u8],
    expiration: u32,
) -> Result<(), MomentoError> {
    // Send SET command: set <key> <flags> <exptime> <bytes>\r\n<data>\r\n
    let key_str = String::from_utf8_lossy(key);
    let cmd = format!("set {} 0 {} {}\r\n", key_str, expiration, data.len());

    // Write command, data, and CRLF in one go for efficiency
    writer
        .write_all(cmd.as_bytes())
        .await
        .map_err(|e| MomentoError {
            message: format!("Failed to send SET command: {}", e),
            error_code: momento::MomentoErrorCode::InternalServerError,
            inner_error: None,
            details: None,
        })?;

    writer.write_all(data).await.map_err(|e| MomentoError {
        message: format!("Failed to send data: {}", e),
        error_code: momento::MomentoErrorCode::InternalServerError,
        inner_error: None,
        details: None,
    })?;

    writer.write_all(b"\r\n").await.map_err(|e| MomentoError {
        message: format!("Failed to send CRLF: {}", e),
        error_code: momento::MomentoErrorCode::InternalServerError,
        inner_error: None,
        details: None,
    })?;

    // Read response
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .await
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

async fn execute_delete(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    key: &[u8],
) -> Result<(), MomentoError> {
    // Send DELETE command
    let key_str = String::from_utf8_lossy(key);
    let cmd = format!("delete {}\r\n", key_str);
    writer
        .write_all(cmd.as_bytes())
        .await
        .map_err(|e| MomentoError {
            message: format!("Failed to send DELETE command: {}", e),
            error_code: momento::MomentoErrorCode::InternalServerError,
            inner_error: None,
            details: None,
        })?;

    // Read response
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .await
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

// Local memcached backend using worker tasks with persistent connections
#[derive(Clone)]
pub struct LocalMemcachedBackend {
    workers: Arc<Vec<mpsc::Sender<Command>>>,
    next_worker: Arc<std::sync::atomic::AtomicUsize>,
}

impl LocalMemcachedBackend {
    pub fn new(servers: Vec<String>) -> Self {
        eprintln!(
            "LocalMemcachedBackend configured with servers: {:?}",
            servers
        );

        let addr = servers
            .first()
            .expect("No servers configured")
            .strip_prefix("memcache://")
            .unwrap_or(servers.first().unwrap())
            .to_string();

        // Create multiple worker tasks (e.g., 100 workers)
        let num_workers = 100;
        let mut workers = Vec::with_capacity(num_workers);

        for _ in 0..num_workers {
            let (tx, rx) = mpsc::channel(100); // Buffer up to 100 commands per worker
            workers.push(tx);

            let addr_clone = addr.clone();
            tokio::spawn(memcached_worker(addr_clone, rx));
        }

        Self {
            workers: Arc::new(workers),
            next_worker: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn get_worker(&self) -> &mpsc::Sender<Command> {
        // Round-robin between workers
        let idx = self
            .next_worker
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.workers.len();
        &self.workers[idx]
    }
}

#[async_trait]
impl CacheBackend for LocalMemcachedBackend {
    async fn get(&self, _cache_name: &str, key: &[u8]) -> Result<GetResponse, MomentoError> {
        let (tx, rx) = oneshot::channel();
        let cmd = Command::Get {
            key: key.to_vec(),
            response: tx,
        };

        self.get_worker()
            .send(cmd)
            .await
            .map_err(|_| MomentoError {
                message: "Worker channel closed".to_string(),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;

        match rx.await {
            Ok(Ok(Some(data))) => {
                // Prepend 4 zero bytes for flags to match momento format
                let mut value = vec![0, 0, 0, 0];
                value.extend_from_slice(&data);
                Ok(GetResponse::Hit { value })
            }
            Ok(Ok(None)) => Ok(GetResponse::Miss),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(MomentoError {
                message: "Worker dropped response channel".to_string(),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            }),
        }
    }

    async fn set(
        &self,
        _cache_name: &str,
        key: Vec<u8>,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> Result<(), MomentoError> {
        let expiration = ttl.map(|d| d.as_secs() as u32).unwrap_or(0);

        // Skip the first 4 bytes (flags) if present
        let data = if value.len() >= 4 {
            value[4..].to_vec()
        } else {
            value
        };

        let (tx, rx) = oneshot::channel();
        let cmd = Command::Set {
            key,
            data,
            expiration,
            response: tx,
        };

        self.get_worker()
            .send(cmd)
            .await
            .map_err(|_| MomentoError {
                message: "Worker channel closed".to_string(),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;

        rx.await.map_err(|_| MomentoError {
            message: "Worker dropped response channel".to_string(),
            error_code: momento::MomentoErrorCode::InternalServerError,
            inner_error: None,
            details: None,
        })?
    }

    async fn delete(&self, _cache_name: &str, key: Vec<u8>) -> Result<(), MomentoError> {
        let (tx, rx) = oneshot::channel();
        let cmd = Command::Delete { key, response: tx };

        self.get_worker()
            .send(cmd)
            .await
            .map_err(|_| MomentoError {
                message: "Worker channel closed".to_string(),
                error_code: momento::MomentoErrorCode::InternalServerError,
                inner_error: None,
                details: None,
            })?;

        rx.await.map_err(|_| MomentoError {
            message: "Worker dropped response channel".to_string(),
            error_code: momento::MomentoErrorCode::InternalServerError,
            inner_error: None,
            details: None,
        })?
    }
}
