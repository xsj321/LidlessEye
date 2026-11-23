//! Pure reductions from API payloads to small, ranked JSON.
//!
//! These functions are deliberately free of I/O so they can be unit tested.
//! The rule they enforce is "summary first": an AI gets totals, units and the
//! handful of rows that explain them, not a 100k-frame flamegraph that crowds
//! out the rest of the conversation.

use lidlesseye_servicemap::{MapHistogram, MapHistory, ServiceMap};
use serde_json::{Value, json};

use crate::client::{array, as_i64, as_str, as_u64, percent};

/// Convert microseconds to milliseconds with two decimals.
pub(crate) fn ms(us: u64) -> f64 {
    (us as f64 / 10.0).round() / 100.0
}

/// Ranked subset of a flamegraph's frames.
pub(crate) fn summarize_flamegraph(raw: &Value, max_frames: usize) -> Value {
    let names = array(Some(raw), "names");
    let levels = array(Some(raw), "levels");
    let values = array(Some(raw), "values");
    let total = as_i64(Some(raw), "total");
    let mut rows: Vec<(usize, i64)> = values
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0)
        .map(|(i, value)| (i, value.as_i64().unwrap_or(0)))
        .collect();
    rows.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| name_at(names, a.0).cmp(&name_at(names, b.0)))
    });
    let truncated = rows.len() > max_frames;
    let frames: Vec<Value> = rows
        .into_iter()
        .take(max_frames)
        .map(|(i, value)| {
            let parent = levels
                .get(i)
                .and_then(Value::as_i64)
                .filter(|p| *p >= 0)
                .and_then(|p| usize::try_from(p).ok())
                .map(|p| name_at(names, p));
            json!({
                "name": name_at(names, i),
                "value": value,
                "share_pct": percent(value as f64, total as f64),
                "parent": parent,
            })
        })
        .collect();
    json!({
        "total": total,
        "samples": as_u64(Some(raw), "samples"),
        "frame_count": names.len(),
        "truncated_by_limit": truncated,
        "top_frames": frames,
    })
}

/// Ranked changes from a two-window diff. Positive deltas are regressions.
pub(crate) fn summarize_diff(raw: &Value, max_frames: usize) -> Value {
    let names = array(Some(raw), "names");
    let levels = array(Some(raw), "levels");
    let left_values = array(Some(raw), "left");
    let right_values = array(Some(raw), "right");
    let deltas = array(Some(raw), "diff");
    let left_total = as_i64(Some(raw), "total_left");
    let right_total = as_i64(Some(raw), "total_right");

    let mut ranked: Vec<(usize, i64)> = deltas
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0)
        .map(|(i, value)| (i, value.as_i64().unwrap_or(0)))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| name_at(names, a.0).cmp(&name_at(names, b.0)))
    });
    let mut improvements: Vec<(usize, i64)> =
        ranked.iter().copied().filter(|(_, d)| *d < 0).collect();
    improvements.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| name_at(names, a.0).cmp(&name_at(names, b.0)))
    });
    let regressions = take_changes(
        &ranked,
        names,
        left_values,
        right_values,
        levels,
        max_frames,
    );
    let improvements = take_changes(
        &improvements,
        names,
        left_values,
        right_values,
        levels,
        max_frames,
    );

    json!({
        "baseline_total": left_total,
        "current_total": right_total,
        "change_pct": percent((right_total - left_total) as f64, left_total as f64),
        "positive_delta_means": "regression",
        "regressions": regressions,
        "improvements": improvements,
    })
}

fn take_changes(
    ranked: &[(usize, i64)],
    names: &[Value],
    left: &[Value],
    right: &[Value],
    levels: &[Value],
    max_frames: usize,
) -> Vec<Value> {
    ranked
        .iter()
        .filter(|(_, delta)| *delta != 0)
        .take(max_frames)
        .map(|(i, delta)| {
            let parent = levels
                .get(*i)
                .and_then(Value::as_i64)
                .filter(|p| *p >= 0)
                .and_then(|p| usize::try_from(p).ok())
                .map(|p| name_at(names, p));
            json!({
                "name": name_at(names, *i),
                "delta": delta,
                "baseline": left.get(*i).and_then(Value::as_i64).unwrap_or(0),
                "current": right.get(*i).and_then(Value::as_i64).unwrap_or(0),
                "parent": parent,
            })
        })
        .collect()
}

/// Per-series totals for a bucketed profiling time series.
pub(crate) fn summarize_timeseries(raw: &Value, max_series: usize) -> Value {
    let series = array(Some(raw), "series");
    let mut rows: Vec<Value> = series
        .iter()
        .map(|entry| {
            let points: Vec<i64> = array(Some(entry), "points")
                .iter()
                .map(|point| point.as_i64().unwrap_or(0))
                .collect();
            let total: i64 = points.iter().sum();
            let peak = points.iter().copied().max().unwrap_or(0);
            let group = entry
                .get("group")
                .and_then(Value::as_str)
                .map(str::to_string);
            json!({
                "service_name": as_str(Some(entry), "service_name"),
                "labels": entry.get("labels").cloned().unwrap_or_else(|| json!({})),
                "group": group,
                "total": total,
                "peak_bucket": peak,
                "mean_bucket": if points.is_empty() { 0.0 } else { round2(total as f64 / points.len() as f64) },
                "buckets": points.len(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        as_i64(Some(b), "total")
            .cmp(&as_i64(Some(a), "total"))
            .then_with(|| as_str(Some(a), "service_name").cmp(&as_str(Some(b), "service_name")))
    });
    let truncated = rows.len() > max_series;
    json!({
        "from_ns": as_u64(Some(raw), "from_ns"),
        "until_ns": as_u64(Some(raw), "until_ns"),
        "step_ns": as_u64(Some(raw), "step_ns"),
        "series_count": rows.len(),
        "truncated_by_limit": truncated,
        "top_series": rows.into_iter().take(max_series).collect::<Vec<_>>(),
    })
}

/// Top services and call/database dependencies of a service map.
pub(crate) fn summarize_service_map(map: &ServiceMap, max_items: usize) -> Value {
    let mut nodes: Vec<&lidlesseye_servicemap::MapNode> = map.nodes.iter().collect();
    nodes.sort_by(|a, b| {
        weight_node(b)
            .cmp(&weight_node(a))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut edges: Vec<&lidlesseye_servicemap::MapEdge> = map.edges.iter().collect();
    edges.sort_by(|a, b| {
        weight_edge(b)
            .cmp(&weight_edge(a))
            .then_with(|| a.client.cmp(&b.client))
            .then_with(|| a.server.cmp(&b.server))
    });
    json!({
        "from_ns": map.from_ns,
        "until_ns": map.until_ns,
        "node_count": map.nodes.len(),
        "edge_count": map.edges.len(),
        "truncated_by_server": map.truncated,
        "top_nodes": nodes.into_iter().take(max_items).map(|node| json!({
            "id": node.id,
            "kind": node.kind.as_str(),
            "connections": node.connections,
            "bytes": node.tx_bytes.saturating_add(node.rx_bytes),
            "retransmits": node.retransmits,
            "cpu_ms": (node.cpu_ns / 1_000_000),
            "pods": node.pods,
            "ready": node.ready,
        })).collect::<Vec<_>>(),
        "top_edges": edges.into_iter().take(max_items).map(|edge| json!({
            "client": edge.client,
            "server": edge.server,
            "protocol": edge.protocol,
            "connections": edge.connections,
            "bytes": edge.tx_bytes.saturating_add(edge.rx_bytes),
            "retransmits": edge.retransmits,
            "rtt_ms": ms(edge.rtt_us),
            "host": edge.host,
        })).collect::<Vec<_>>(),
    })
}

fn weight_node(node: &lidlesseye_servicemap::MapNode) -> u128 {
    u128::from(node.connections)
        + u128::from(node.tx_bytes.saturating_add(node.rx_bytes)) / 1024
        + u128::from(node.cpu_ns) / 1_000_000
}

fn weight_edge(edge: &lidlesseye_servicemap::MapEdge) -> u128 {
    u128::from(edge.connections) + u128::from(edge.tx_bytes.saturating_add(edge.rx_bytes)) / 1024
}

/// Totals of a node's own and incident history buckets.
pub(crate) fn summarize_map_history(history: &MapHistory) -> Value {
    let own = summarize_history_points(&history.points);
    let edges = summarize_history_points(&history.edges);
    json!({
        "node": history.node,
        "from_ns": history.from_ns,
        "until_ns": history.until_ns,
        "step_ns": history.step_ns,
        "own": {
            "buckets": own.len(),
            "connections": own.iter().map(|p| as_u64(Some(p), "connections")).sum::<u64>(),
            "bytes": own.iter().map(|p| as_u64(Some(p), "bytes")).sum::<u64>(),
            "retransmits": own.iter().map(|p| as_u64(Some(p), "retransmits")).sum::<u64>(),
            "peak_connections": own.iter().map(|p| as_u64(Some(p), "connections")).max().unwrap_or(0),
            "points": own,
        },
        "edges": {
            "buckets": edges.len(),
            "connections": edges.iter().map(|p| as_u64(Some(p), "connections")).sum::<u64>(),
            "bytes": edges.iter().map(|p| as_u64(Some(p), "bytes")).sum::<u64>(),
            "retransmits": edges.iter().map(|p| as_u64(Some(p), "retransmits")).sum::<u64>(),
            "peak_rtt_ms": edges.iter().map(|p| p.get("rtt_ms").and_then(Value::as_f64).unwrap_or(0.0)).fold(0.0_f64, f64::max),
            "points": edges,
        },
    })
}

fn summarize_history_points(points: &[lidlesseye_servicemap::MapHistoryPoint]) -> Vec<Value> {
    points
        .iter()
        .map(|point| {
            json!({
                "bucket_ns": point.bucket_ns,
                "connections": point.connections,
                "bytes": point.tx_bytes.saturating_add(point.rx_bytes),
                "retransmits": point.retransmits,
                "rtt_ms": ms(point.rtt_us),
                "pods": point.pods,
                "ready": point.ready,
            })
        })
        .collect()
}

/// Compact density series: explicit bucket count and peak position.
pub(crate) fn summarize_histogram(histogram: &MapHistogram, max_buckets: usize) -> Value {
    let points = &histogram.points;
    let total: u64 = points.iter().sum();
    let peak = points
        .iter()
        .enumerate()
        .max_by_key(|(_, value)| **value)
        .map(|(index, value)| json!({ "index": index, "value": value }))
        .unwrap_or_else(|| json!({ "index": 0, "value": 0 }));
    let truncated = points.len() > max_buckets;
    let step = if points.len() <= max_buckets {
        1
    } else {
        points.len().div_ceil(max_buckets)
    };
    let sampled: Vec<Value> = points
        .iter()
        .step_by(step)
        .enumerate()
        .map(|(i, value)| json!({ "index": i * step, "value": value }))
        .collect();
    json!({
        "metric": histogram.metric,
        "from_ns": histogram.from_ns,
        "until_ns": histogram.until_ns,
        "step_ns": histogram.step_ns,
        "bucket_count": points.len(),
        "total": total,
        "mean_per_bucket": if points.is_empty() { 0.0 } else { round2(total as f64 / points.len() as f64) },
        "peak": peak,
        "truncated_by_limit": truncated,
        "sampled_buckets": sampled,
    })
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn name_at(names: &[Value], index: usize) -> String {
    names
        .get(index)
        .and_then(Value::as_str)
        .unwrap_or("<unknown>")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flamegraph_summary_ranks_frames_and_keeps_percentages() {
        let raw = json!({
            "names": ["root", "leaf", "mid"],
            "levels": [-1, 0, 1],
            "values": [10, 7, 3],
            "samples": 10,
            "total": 10,
        });
        let result = summarize_flamegraph(&raw, 2);
        assert_eq!(result["top_frames"][0]["name"], "leaf");
        assert_eq!(result["top_frames"][0]["share_pct"], 70.0);
        assert_eq!(result["frame_count"], 3);
    }

    #[test]
    fn diff_summary_labels_positive_delta_as_regression() {
        let raw = json!({
            "names": ["root", "slower", "faster"],
            "levels": [-1, 0, 0],
            "left": [10, 3, 5],
            "right": [10, 8, 1],
            "diff": [0, 5, -4],
            "total_left": 10,
            "total_right": 10,
        });
        let result = summarize_diff(&raw, 10);
        assert_eq!(result["regressions"][0]["name"], "slower");
        assert_eq!(result["improvements"][0]["name"], "faster");
        assert_eq!(result["change_pct"], 0.0);
    }

    #[test]
    fn histogram_summary_samples_without_losing_the_peak() {
        let histogram = MapHistogram {
            metric: "connections".to_string(),
            points: vec![0, 4, 0, 9, 0],
            ..Default::default()
        };
        let result = summarize_histogram(&histogram, 2);
        assert_eq!(result["total"], 13);
        assert_eq!(result["peak"]["index"], 3);
        assert_eq!(result["peak"]["value"], 9);
        assert_eq!(result["truncated_by_limit"], true);
    }

    #[test]
    fn microseconds_are_reported_as_milliseconds() {
        assert_eq!(ms(1_500), 1.5);
    }
}
