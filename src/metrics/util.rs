use goodmetrics::{GaugeDimensions, GaugeFactory, HistogramHandle, StatisticSetHandle, SumHandle};

pub fn proxy_sum_gauge(g: &GaugeFactory, name: &'static str) -> SumHandle {
    g.dimensioned_gauge_sum("momento_proxy", name, Default::default())
}

pub fn proxy_statistic_set_gauge(g: &GaugeFactory, name: &'static str) -> StatisticSetHandle {
    g.dimensioned_gauge_statistic_set("momento_proxy", name, Default::default())
}

fn proxy_request_latency_histogram(
    gauge_factory: &GaugeFactory,
    rpc: &'static str,
    result: &'static str,
) -> HistogramHandle {
    gauge_factory.dimensioned_gauge_histogram(
        "momento_proxy",
        "latency",
        GaugeDimensions::new([("rpc", rpc), ("result", result)]),
    )
}

fn proxy_hit_response_latency_histogram(
    gauge_factory: &GaugeFactory,
    rpc: &'static str,
    result: &'static str,
    source: &'static str, // momento or mcache
) -> HistogramHandle {
    gauge_factory.dimensioned_gauge_histogram(
        "momento_proxy",
        "latency",
        GaugeDimensions::new([("rpc", rpc), ("result", result), ("source", source)]),
    )
}

pub fn proxy_request_latency_hit_histogram(
    g: &GaugeFactory,
    rpc: &'static str,
    source: &'static str,
) -> HistogramHandle {
    proxy_hit_response_latency_histogram(g, rpc, "hit", source)
}

pub fn proxy_request_latency_miss_histogram(
    g: &GaugeFactory,
    rpc: &'static str,
) -> HistogramHandle {
    proxy_request_latency_histogram(g, rpc, "miss")
}

pub fn proxy_request_latency_ok_histogram(g: &GaugeFactory, rpc: &'static str) -> HistogramHandle {
    proxy_request_latency_histogram(g, rpc, "ok")
}

pub fn proxy_request_latency_error_histogram(
    gauge_factory: &GaugeFactory,
    rpc: &'static str,
) -> HistogramHandle {
    proxy_request_latency_histogram(gauge_factory, rpc, "error")
}

pub fn proxy_request_latency_timeout_histogram(
    gauge_factory: &GaugeFactory,
    rpc: &'static str,
) -> HistogramHandle {
    proxy_request_latency_histogram(gauge_factory, rpc, "timeout")
}
