use crate::default_buffer_size;
use crate::PAGESIZE;
use core::num::NonZeroU64;
use std::net::AddrParseError;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::time::Duration;

use config::Admin;
use config::AdminConfig;
use config::Debug;
use config::DebugConfig;
use config::Klog;
use config::KlogConfig;
use serde::{Deserialize, Serialize};

use std::io::Read;

#[derive(Copy, Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Memcache,
    Resp,
}

impl Default for Protocol {
    fn default() -> Self {
        Self::Memcache
    }
}

// support for memcache flags is on by default
fn flags() -> bool {
    true
}

// struct definitions
#[derive(Clone, Serialize, Default, Deserialize, Debug)]
pub struct MomentoProxyConfig {
    // application modules
    #[serde(default)]
    admin: Admin,
    #[serde(default)]
    proxy: Proxy,
    #[serde(default)]
    cache: Vec<Cache>,
    #[serde(default)]
    debug: Debug,
    #[serde(default)]
    klog: Klog,
}

#[derive(Default, Clone, Copy, Serialize, Deserialize, Debug)]
pub struct Proxy {
    threads: Option<usize>,
}

// definitions
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Cache {
    host: String,
    port: String,
    cache_name: String,
    default_ttl: NonZeroU64,
    #[serde(default = "four")]
    connection_count: NonZeroUsize,
    #[serde(default)]
    protocol: Protocol,
    #[serde(default = "flags")]
    flags: bool,
    /// 0 to disable
    #[serde(default)]
    memory_cache_bytes: usize,
    /// 0 means no expiration
    #[serde(default)]
    memory_cache_ttl_seconds: u64,
    #[serde(default = "default_buffer_size")]
    buffer_size: NonZeroUsize,
}

const fn four() -> NonZeroUsize {
    NonZeroUsize::new(4).expect("4 is nonzero")
}

// implementation
impl Cache {
    /// Host address to listen on
    pub fn host(&self) -> String {
        self.host.clone()
    }

    /// Port to listen on
    pub fn port(&self) -> String {
        self.port.clone()
    }

    /// Return the result of parsing the host and port
    pub fn socket_addr(&self) -> Result<SocketAddr, AddrParseError> {
        format!("{}:{}", self.host(), self.port()).parse()
    }

    /// Returns the name of the momento cache that requests will be sent to
    pub fn cache_name(&self) -> String {
        self.cache_name.clone()
    }

    /// The default TTL (in seconds) for
    pub fn default_ttl(&self) -> Duration {
        Duration::from_secs(self.default_ttl.get())
    }

    pub fn connection_count(&self) -> usize {
        self.connection_count.get()
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    pub fn flags(&self) -> bool {
        self.flags
    }

    /// 0 to disable
    pub fn memory_cache_bytes(&self) -> usize {
        self.memory_cache_bytes
    }
    /// 0 means no expiration
    pub fn memory_cache_ttl_seconds(&self) -> u64 {
        self.memory_cache_ttl_seconds
    }

    pub fn buffer_size(&self) -> usize {
        // rounds the buffer size up to the next nearest multiple of the
        // pagesize
        std::cmp::max(1, self.buffer_size.get()).div_ceil(PAGESIZE)
    }
}

// implementation
impl MomentoProxyConfig {
    pub fn load(file: &str) -> Result<Self, std::io::Error> {
        let mut file = std::fs::File::open(file)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        match toml::from_str(&content) {
            Ok(t) => Ok(t),
            Err(e) => {
                eprintln!("{e}");
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Error parsing config",
                ))
            }
        }
    }

    pub fn caches(&self) -> &[Cache] {
        &self.cache
    }

    pub fn threads(&self) -> Option<usize> {
        self.proxy.threads
    }
}

impl AdminConfig for MomentoProxyConfig {
    fn admin(&self) -> &Admin {
        &self.admin
    }
}

impl DebugConfig for MomentoProxyConfig {
    fn debug(&self) -> &Debug {
        &self.debug
    }
}

impl KlogConfig for MomentoProxyConfig {
    fn klog(&self) -> &Klog {
        &self.klog
    }
}
