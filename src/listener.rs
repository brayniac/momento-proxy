// Copyright 2022 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use crate::cache_backend::CacheBackend;
use crate::*;
use momento::CacheClient;
use momento_proxy::Protocol;
use pelikan_net::{TCP_ACCEPT, TCP_CLOSE, TCP_CONN_CURR};

pub(crate) async fn listener<B: CacheBackend>(
    listener: TcpListener,
    backend: B,
    cache_name: String,
    protocol: Protocol,
    flags: bool,
    proxy_metrics: impl ProxyMetrics,
    memory_cache: Option<MCache>,
    buffer_size: usize,
) {
    // Currently only memcache protocol supports backends
    if matches!(protocol, Protocol::Resp) {
        error!("RESP protocol with custom backends is not yet supported");
        return;
    }
    // this acts as our listener thread and spawns tasks for each client
    loop {
        // accept a new client
        if let Ok((socket, _)) = listener.accept().await {
            TCP_ACCEPT.increment();

            let backend = backend.clone();
            let cache_name = cache_name.clone();

            // spawn a task for managing requests for the client
            let proxy_metrics = proxy_metrics.clone();
            let memory_cache = memory_cache.clone();

            tokio::spawn(async move {
                TCP_CONN_CURR.increment();
                let _connection_metric = proxy_metrics.begin_connection();

                // We already checked protocol is Memcache above
                crate::frontend::handle_memcache_client(
                    socket,
                    backend,
                    cache_name,
                    flags,
                    proxy_metrics,
                    memory_cache,
                    buffer_size,
                )
                .await;

                TCP_CONN_CURR.decrement();
                TCP_CLOSE.increment();
            });
        }
    }
}

// Separate listener for RESP protocol that still uses CacheClient directly
pub(crate) async fn resp_listener(
    listener: TcpListener,
    client: CacheClient,
    cache_name: String,
    proxy_metrics: impl ProxyMetrics,
    buffer_size: usize,
) {
    // this acts as our listener thread and spawns tasks for each client
    loop {
        // accept a new client
        if let Ok((socket, _)) = listener.accept().await {
            TCP_ACCEPT.increment();

            let client = client.clone();
            let cache_name = cache_name.clone();

            // spawn a task for managing requests for the client
            let proxy_metrics = proxy_metrics.clone();

            tokio::spawn(async move {
                TCP_CONN_CURR.increment();
                let _connection_metric = proxy_metrics.begin_connection();

                crate::frontend::handle_resp_client(
                    socket,
                    client,
                    cache_name,
                    proxy_metrics,
                    buffer_size,
                )
                .await;

                TCP_CONN_CURR.decrement();
                TCP_CLOSE.increment();
            });
        }
    }
}
