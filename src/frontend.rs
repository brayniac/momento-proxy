// Copyright 2022 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use crate::protocol::*;
use crate::*;
use crate::cache_backend::CacheBackend;
use pelikan_net::TCP_SEND_BYTE;
use protocol_memcache::Protocol;
use session::Buf;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) async fn handle_memcache_client<B: CacheBackend>(
    socket: tokio::net::TcpStream,
    client: B,
    cache_name: String,
    flags: bool,
    proxy_metrics: impl ProxyMetrics,
    memory_cache: Option<MCache>,
    buffer_size: usize,
) {
    debug!("accepted memcache client, waiting for first byte to detect text or binary");

    let mut buf = [0];

    loop {
        match socket.peek(&mut buf).await {
            Ok(0) => {
                // client hangup
                return;
            }
            Ok(_) => {
                // check which protocol we use
                if buf[0] == 0x80 {
                    debug!("accepted memcache binary client");
                    handle_memcache_client_concrete(
                        socket,
                        client,
                        cache_name,
                        protocol_memcache::BinaryProtocol::default(),
                        flags,
                        proxy_metrics,
                        memory_cache,
                        buffer_size,
                    )
                    .await;
                    return;
                } else {
                    debug!("accepted memcache text client");
                    handle_memcache_client_concrete(
                        socket,
                        client,
                        cache_name,
                        protocol_memcache::TextProtocol::default(),
                        flags,
                        proxy_metrics,
                        memory_cache,
                        buffer_size,
                    )
                    .await;
                    return;
                };
            }
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    // spurious wakeup
                    continue;
                } else {
                    // some unknown error
                    return;
                }
            }
        }
    }
}

pub(crate) async fn handle_memcache_client_concrete<B: CacheBackend>(
    socket: tokio::net::TcpStream,
    client: B,
    cache_name: String,
    protocol: impl Protocol<protocol_memcache::Request, protocol_memcache::Response>
        + Clone
        + Send
        + 'static,
    flags: bool,
    proxy_metrics: impl ProxyMetrics,
    memory_cache: Option<MCache>,
    buffer_size: usize,
) {
    // initialize a buffer for incoming bytes from the client
    let mut read_buffer = Buffer::new(buffer_size);
    let mut write_buffer = Buffer::new(buffer_size);

    // initialize the protocol
    let protocol2 = protocol.clone();

    // queue for response passing back from tasks
    let (sender, mut receiver) = mpsc::channel::<
        std::io::Result<(u64, protocol_memcache::Request, protocol_memcache::Response)>,
    >(1024);

    let (mut read_half, mut write_half) = socket.into_split();

    let sequence = Arc::new(AtomicU64::new(0));
    let sequence2 = sequence.clone();

    let read_alive = Arc::new(AtomicBool::new(true));
    let read_alive2 = read_alive.clone();

    let write_alive = Arc::new(AtomicBool::new(true));
    let write_alive2 = write_alive.clone();

    tokio::spawn(async move {
        let mut next_sequence: u64 = 0;
        let mut backlog = BTreeMap::new();

        while write_alive2.load(Ordering::Relaxed) {
            if !read_alive2.load(Ordering::Relaxed)
                && next_sequence == sequence2.load(Ordering::Relaxed)
            {
                write_alive2.store(false, Ordering::Relaxed);
                return;
            }

            debug!("writer loop");
            if let Some(result) = receiver.recv().await {
                match result {
                    Ok((sequence, request, response)) => {
                        if sequence == next_sequence {
                            debug!("sending next: {next_sequence}");
                            next_sequence += 1;
                            if protocol2
                                .compose_response(&request, &response, &mut write_buffer)
                                .is_err()
                            {
                                read_alive2.store(false, Ordering::Relaxed);
                                write_alive2.store(false, Ordering::Relaxed);
                                return;
                            }

                            'backlog: while !backlog.is_empty() {
                                if let Some((request, response)) = backlog.remove(&next_sequence) {
                                    debug!("sending next: {next_sequence}");
                                    next_sequence += 1;
                                    if protocol2
                                        .compose_response(&request, &response, &mut write_buffer)
                                        .is_err()
                                    {
                                        read_alive2.store(false, Ordering::Relaxed);
                                        write_alive2.store(false, Ordering::Relaxed);
                                        return;
                                    }
                                } else {
                                    break 'backlog;
                                }
                            }
                        } else {
                            debug!("queueing seq: {sequence}");
                            backlog.insert(sequence, (request, response));
                        }
                    }
                    Err(_e) => {
                        read_alive2.store(false, Ordering::Relaxed);
                        write_alive2.store(false, Ordering::Relaxed);
                        return;
                    }
                }
            }

            while write_buffer.remaining() > 0 {
                debug!("non-blocking write");
                if do_write2(&mut write_half, &mut write_buffer).await.is_err() {
                    read_alive2.store(false, Ordering::Relaxed);
                    write_alive2.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
    });

    // loop to handle the connection
    while read_alive.load(Ordering::Relaxed) {
        // read data from the tcp stream into the buffer
        if do_read2(&mut read_half, &mut read_buffer).await.is_err() {
            // any read errors result in hangup
            read_alive.store(false, Ordering::Relaxed);
        }

        // dispatch all complete requests in the socket buffer as async tasks
        //
        // NOTE: errors in the request handlers typically indicate write errors.
        //       To eliminate possibility for desync, we hangup if there is an
        //       error. The request handlers should implement graceful handling
        //       of backend errors.
        'requests: loop {
            let borrowed_buf = read_buffer.borrow();

            match protocol.parse_request(borrowed_buf) {
                Ok(request) => {
                    debug!("read request");

                    let consumed = request.consumed();
                    let request = request.into_inner();

                    read_buffer.advance(consumed);

                    let sender = sender.clone();
                    let client = client.clone();
                    let cache_name = cache_name.clone();

                    let sequence = sequence.fetch_add(1, Ordering::Relaxed);

                    let proxy_metrics = proxy_metrics.clone();
                    let memory_cache = memory_cache.clone();
                    tokio::spawn(async move {
                        handle_memcache_request(
                            sender,
                            client,
                            cache_name,
                            sequence,
                            request,
                            flags,
                            proxy_metrics,
                            memory_cache,
                        )
                        .await;
                    });
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        // more data needs to be read from the stream, so stop
                        // processing requests
                        break 'requests;
                    }
                    _ => {
                        // invalid request
                        trace!("malformed request: {:?}", borrowed_buf);
                        read_alive.store(false, Ordering::Relaxed);
                        return;
                    }
                },
            }
        }
    }

    for _ in 0..60 {
        if write_alive.load(Ordering::Relaxed) {
            // time delay for write half to complete
            tokio::time::sleep(Duration::from_secs(1)).await;
        } else {
            break;
        }
    }

    // shutdown write half
    write_alive.store(false, Ordering::Relaxed);
}

// The memcached protocol expects us to return a reponse corresponding to
// one of the enums, but we need the RpcGuard to report an error is the
// response is actually an error.
impl ResponseWrappingError for protocol_memcache::Response {
    fn is_error(&self) -> bool {
        match self {
            protocol_memcache::Response::ServerError(_) => true,
            protocol_memcache::Response::ClientError(_) => true,
            protocol_memcache::Response::Error(_) => true,
            _ => false,
        }
    }
}

async fn handle_memcache_request<B: CacheBackend>(
    channel: mpsc::Sender<
        std::result::Result<
            (u64, protocol_memcache::Request, protocol_memcache::Response),
            std::io::Error,
        >,
    >,
    mut client: B,
    cache_name: String,
    sequence: u64,
    request: protocol_memcache::Request,
    flags: bool,
    proxy_metrics: impl ProxyMetrics,
    memory_cache: Option<MCache>,
) {
    let result = match request {
        memcache::Request::Delete(ref r) => {
            if let Some(memory_cache) = memory_cache {
                memory_cache.delete(r.key());
            }
            with_wrapped_error_response_rpc_call_guard(
                proxy_metrics.begin_memcached_delete(),
                memcache::delete(&mut client, &cache_name, r),
            )
            .await
        }
        memcache::Request::Get(ref r) => {
            let recorder = proxy_metrics.begin_memcached_get();
            with_wrapped_error_response_rpc_call_guard(
                recorder.clone(),
                memcache::get(&mut client, &cache_name, r, flags, memory_cache, &recorder),
            )
            .await
        }
        memcache::Request::Set(ref r) => {
            with_wrapped_error_response_rpc_call_guard(
                proxy_metrics.begin_memcached_set(),
                memcache::set(&mut client, &cache_name, r, flags, memory_cache),
            )
            .await
        }
        _ => {
            debug!("unsupported command: {}", request);
            with_rpc_call_guard(proxy_metrics.begin_memcached_unimplemented(), async {
                Err(Error::new(ErrorKind::Other, "unsupported"))
            })
            .await
        }
    };

    match result {
        Ok(response) => {
            let _ = channel.send(Ok((sequence, request, response))).await;
        }
        Err(e) => {
            let _ = channel.send(Err(e)).await;
        }
    }
}

pub(crate) async fn handle_resp_client(
    mut socket: tokio::net::TcpStream,
    mut client: CacheClient,
    cache_name: String,
    proxy_metrics: impl RespMetrics,
    buffer_size: usize,
) {
    debug!("accepted resp client");

    // initialize a buffer for incoming bytes from the client
    let mut buf = Buffer::new(buffer_size);

    // initialize the request parser
    let parser = resp::RequestParser::new();

    // handle incoming data from the client
    loop {
        if do_read(&mut socket, &mut buf).await.is_err() {
            break;
        }

        let borrowed_buf = buf.borrow();

        let request = match parser.parse(borrowed_buf) {
            Ok(request) => request,
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => continue,
                _ => {
                    trace!("malformed request: {:?}", borrowed_buf);
                    let _ = socket.write_all(b"-ERR malformed request\r\n").await;
                    break;
                }
            },
        };

        let consumed = request.consumed();
        let request = request.into_inner();
        let command = request.command();

        let mut response_buf = Vec::<u8>::new();

        let result: ProxyResult = async {
            match &request {
                resp::Request::Del(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_del(),
                        resp::del(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::Get(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_get(),
                        resp::get(&mut client, &cache_name, &mut response_buf, r.key()),
                    )
                    .await?
                }

                resp::Request::HashDelete(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hdel(),
                        resp::hdel(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashExists(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hexists(),
                        resp::hexists(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashGet(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hget(),
                        resp::hget(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashGetAll(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hgetall(),
                        resp::hgetall(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashIncrBy(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hincrby(),
                        resp::hincrby(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashKeys(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hkeys(),
                        resp::hkeys(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashLength(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hlen(),
                        resp::hlen(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashMultiGet(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hmget(),
                        resp::hmget(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashSet(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hset(),
                        resp::hset(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::HashValues(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_hvals(),
                        resp::hvals(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::ListIndex(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_lindex(),
                        resp::lindex(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::ListLen(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_llen(),
                        resp::llen(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::ListPop(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_lpop(),
                        resp::lpop(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::ListRange(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_lrange(),
                        resp::lrange(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::ListPush(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_lpush(),
                        resp::lpush(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::ListPushBack(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_rpush(),
                        resp::rpush(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::ListPopBack(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_rpop(),
                        resp::rpop(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::Set(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_set(),
                        resp::set(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::SetAdd(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_sadd(),
                        resp::sadd(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::SetRem(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_srem(),
                        resp::srem(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::SetDiff(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_sdiff(),
                        resp::sdiff(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::SetUnion(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_sunion(),
                        resp::sunion(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::SetIntersect(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_sinter(),
                        resp::sinter(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }

                resp::Request::SetMembers(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_smembers(),
                        resp::smembers(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SetIsMember(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_sismember(),
                        resp::sismember(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetCardinality(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zcard(),
                        resp::zcard(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetIncrement(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zincrby(),
                        resp::zincrby(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetScore(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zscore(),
                        resp::zscore(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetMultiScore(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zmscore(),
                        resp::zmscore(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetRemove(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zrem(),
                        resp::zrem(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetRank(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zrank(),
                        resp::zrank(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetRange(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zrange(),
                        resp::zrange(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetAdd(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zadd(),
                        resp::zadd(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetReverseRank(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zrevrank(),
                        resp::zrevrank(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetCount(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zcount(),
                        resp::zcount(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                resp::Request::SortedSetUnionStore(r) => {
                    with_rpc_call_guard(
                        proxy_metrics.begin_resp_zunionstore(),
                        resp::zunionstore(&mut client, &cache_name, &mut response_buf, r),
                    )
                    .await?
                }
                _ => {
                    debug!("unsupported command: {}", command);
                    with_rpc_call_guard(proxy_metrics.begin_resp_unimplemented(), async {
                        Err(ProxyError::UnsupportedCommand(request.command()))
                    })
                    .await?
                }
            }

            Ok(())
        }
        .await;

        let fatal = match result {
            Ok(()) => false,
            Err(e) => {
                response_buf.clear();

                match e {
                    ProxyError::Momento(error) => {
                        SESSION_SEND.increment();
                        crate::protocol::resp::momento_error_to_resp_error(
                            &mut response_buf,
                            command,
                            error,
                        );

                        false
                    }
                    ProxyError::Timeout(_) => {
                        SESSION_SEND.increment();
                        BACKEND_EX.increment();
                        BACKEND_EX_TIMEOUT.increment();
                        response_buf.extend_from_slice(b"-ERR backend timeout\r\n");

                        false
                    }
                    ProxyError::Io(_) => true,
                    ProxyError::UnsupportedCommand(command) => {
                        debug!("unsupported resp command: {command}");
                        response_buf.extend_from_slice(
                            format!("-ERR unsupported command: {command}\r\n").as_bytes(),
                        );
                        true
                    }
                    ProxyError::Custom(message) => {
                        SESSION_SEND.increment();
                        BACKEND_EX.increment();
                        response_buf.extend_from_slice(b"-ERR ");
                        response_buf.extend_from_slice(message.as_bytes());
                        response_buf.extend_from_slice(b"\r\n");

                        true
                    }
                }
            }
        };

        // Temporary workaround
        // ====================
        // There are a few metrics that are incremented on every request. Before the
        // refactor, these were incremented within each call. Now, they should be
        // handled in this function. As an intermediate, we increment only if the request
        // method put data into response_buf.
        if !response_buf.is_empty() {
            BACKEND_REQUEST.increment();
            SESSION_SEND.increment();
        }

        SESSION_SEND_BYTE.add(response_buf.len() as _);
        TCP_SEND_BYTE.add(response_buf.len() as _);

        if socket.write_all(&response_buf).await.is_err() {
            SESSION_SEND_EX.increment();
            break;
        }

        if fatal {
            break;
        }

        buf.advance(consumed);
    }
}
