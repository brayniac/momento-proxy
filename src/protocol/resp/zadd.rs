// Copyright 2025 Pelikan Foundation LLC.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use std::io::Write;
use std::time::Duration;

use momento::cache::SortedSetElement;
use momento::CacheClient;
use protocol_resp::{SortedSetAdd, SortedSetIncrement, ZADD, ZADD_EX};
use tokio::time;

use crate::error::ProxyResult;
use crate::klog::{klog_1, Status};
use crate::ProxyError;

use super::{update_method_metrics, zincrby};

pub async fn zadd(
    client: &mut CacheClient,
    cache_name: &str,
    response_buf: &mut Vec<u8>,
    req: &SortedSetAdd,
) -> ProxyResult {
    update_method_metrics(&ZADD, &ZADD_EX, async move {
        let number_of_elements_added = req.members().len();

        // Momento does not yet support some of these optional arguments, return an error if any are set
        if req.optional_args().ch
            || req.optional_args().xx
            || req.optional_args().nx
            || req.optional_args().gt
            || req.optional_args().lt
        {
            klog_1(&"zadd", &req.key(), Status::ServerError, 0);
            return Err(ProxyError::from(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Momento proxy does not support CH, XX, NX, GT, or LT optional arguments",
            )));
        }

        // If INCR is set, then ZADD should behave like ZINCRBY (as per the docs), which accepts only a single score-member pair
        if req.optional_args().incr {
            let zincrby_request =
                SortedSetIncrement::new(req.key(), req.members()[0].0, &req.members()[0].1);
            zincrby(client, cache_name, response_buf, &zincrby_request).await?;
            return Ok(());
        }

        // Otherwise it's a regular ZADD call
        let mut converted_members: Vec<SortedSetElement<Vec<u8>>> = Vec::new();
        for element in req.members() {
            converted_members.push(SortedSetElement {
                value: element.1.to_vec(),
                score: if element.0 == f64::INFINITY {
                    f64::MAX
                } else if element.0 == f64::NEG_INFINITY {
                    f64::MIN
                } else {
                    element.0
                },
            })
        }

        match time::timeout(
            Duration::from_millis(200),
            client.sorted_set_put_elements(cache_name, req.key(), converted_members),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                klog_1(&"zadd", &req.key(), Status::ServerError, 0);
                return Err(ProxyError::from(e));
            }
            Err(e) => {
                klog_1(&"zadd", &req.key(), Status::Timeout, 0);
                return Err(ProxyError::from(e));
            }
        };

        // If there was no error, we assume all the elements were added and return the number of elements added
        write!(response_buf, ":{}\r\n", number_of_elements_added)?;
        klog_1(&"zadd", &req.key(), Status::Hit, response_buf.len());

        Ok(())
    })
    .await
}
