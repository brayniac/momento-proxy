// Copyright 2025 Pelikan Foundation LLC.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use std::time::Duration;

use momento::cache::{
    ScoreBound, SortedSetFetchByScoreRequest, SortedSetFetchResponse, SortedSetOrder,
};
use momento::CacheClient;
use protocol_resp::RangeType;
use protocol_resp::{SortedSetRange, ZRANGE, ZRANGE_EX, ZRANGE_HIT, ZRANGE_MISS};
use tokio::time;

use crate::error::ProxyResult;
use crate::klog::{klog_1, Status};
use crate::ProxyError;

use super::{
    parse_score_boundary_as_float, parse_score_boundary_as_integer, update_method_metrics,
};

pub async fn zrange(
    client: &mut CacheClient,
    cache_name: &str,
    response_buf: &mut Vec<u8>,
    req: &SortedSetRange,
) -> ProxyResult {
    update_method_metrics(&ZRANGE, &ZRANGE_EX, async move {
        if *req.range_type() == RangeType::ByLex {
            klog_1(&"zrange", &req.key(), Status::ServerError, 0);
            return Err(ProxyError::from(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Momento proxy does not support BYLEX for ZRANGE",
            )));
        }

        let response = match *req.range_type() {
            RangeType::ByIndex => {
                // ByIndex accepts only integers as inclusive start and inclusive stop values
                let start = parse_score_boundary_as_integer(req.start())?;
                let stop = parse_score_boundary_as_integer(req.stop())?;

                let order = match req.optional_args().reversed {
                    Some(true) => SortedSetOrder::Descending,
                    _ => SortedSetOrder::Ascending,
                };

                match time::timeout(
                    Duration::from_millis(200),
                    client.sorted_set_fetch_by_rank(
                        cache_name,
                        req.key(),
                        order,
                        Some(start),
                        Some(stop),
                    ),
                )
                .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        klog_1(&"zrange", &req.key(), Status::ServerError, 0);
                        return Err(ProxyError::from(e));
                    }
                    Err(e) => {
                        klog_1(&"zrange", &req.key(), Status::Timeout, 0);
                        return Err(ProxyError::from(e));
                    }
                }
            }
            RangeType::ByScore => {
                // ByScore can accept start/stop values: `(` for exclusive boundary, `+inf`, `-inf`.
                let (start, exclusive_start) = parse_score_boundary_as_float(req.start())?;
                let (stop, exclusive_stop) = parse_score_boundary_as_float(req.stop())?;

                let order = match req.optional_args().reversed {
                    Some(true) => SortedSetOrder::Descending,
                    _ => SortedSetOrder::Ascending,
                };

                let min_score = match (start, exclusive_start) {
                    (f64::NEG_INFINITY, _) => None,
                    (f64::INFINITY, _) => Some(ScoreBound::Inclusive(f64::MAX)),
                    (score, true) => Some(ScoreBound::Exclusive(score)),
                    (score, false) => Some(ScoreBound::Inclusive(score)),
                };

                let max_score = match (stop, exclusive_stop) {
                    (f64::INFINITY, _) => None,
                    (f64::NEG_INFINITY, _) => Some(ScoreBound::Exclusive(f64::MIN)),
                    (score, true) => Some(ScoreBound::Exclusive(score)),
                    (score, false) => Some(ScoreBound::Inclusive(score)),
                };

                let fetch_request = SortedSetFetchByScoreRequest::new(cache_name, req.key())
                    .order(order)
                    .min_score(min_score)
                    .max_score(max_score)
                    .offset(req.optional_args().offset.map(|o| o as u32))
                    .count(req.optional_args().count.map(|c| c as i32));

                match time::timeout(
                    Duration::from_millis(200),
                    client.send_request(fetch_request),
                )
                .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        klog_1(&"zrange", &req.key(), Status::ServerError, 0);
                        return Err(ProxyError::from(e));
                    }
                    Err(e) => {
                        klog_1(&"zrange", &req.key(), Status::Timeout, 0);
                        return Err(ProxyError::from(e));
                    }
                }
            }
            _ => {
                klog_1(&"zrange", &req.key(), Status::ServerError, 0);
                return Err(ProxyError::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "malformed command",
                )));
            }
        };

        let include_scores = matches!(req.optional_args().with_scores, Some(true));
        match response {
            SortedSetFetchResponse::Hit { value } => {
                ZRANGE_HIT.increment();

                if include_scores {
                    // Return elements and scores
                    response_buf
                        .extend_from_slice(format!("*{}\r\n", value.elements.len() * 2).as_bytes());
                } else {
                    // Return elements only
                    response_buf
                        .extend_from_slice(format!("*{}\r\n", value.elements.len()).as_bytes());
                }

                for (element, score) in value.elements {
                    response_buf.extend_from_slice(format!("${}\r\n", element.len()).as_bytes());
                    response_buf.extend_from_slice(&element);
                    response_buf.extend_from_slice(b"\r\n");

                    if include_scores {
                        response_buf.extend_from_slice(
                            format!("${}\r\n", score.to_string().len()).as_bytes(),
                        );
                        response_buf.extend_from_slice(score.to_string().as_bytes());
                        response_buf.extend_from_slice(b"\r\n");
                    }
                }
                klog_1(&"zrange", &req.key(), Status::Hit, response_buf.len());
            }
            SortedSetFetchResponse::Miss => {
                ZRANGE_MISS.increment();
                // return empty list on miss
                response_buf.extend_from_slice(b"*0\r\n");
                klog_1(&"zrange", &req.key(), Status::Miss, response_buf.len());
            }
        }

        Ok(())
    })
    .await
}
