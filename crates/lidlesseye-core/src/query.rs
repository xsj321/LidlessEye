//! The query engine: merge segments into flamegraphs, top-N tables and time
//! series.
//!
//! Everything here works on the mapped segment arrays directly. A query over
//! N segments allocates one `Vec<Frame>` for the frame dictionary, one entry
//! per node of the resulting tree, and nothing per sample.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::error::{Error, Result};
use crate::model::{Frame, ProfileType, Unit};
use crate::segment::Segment;
use crate::selector::Selector;
use crate::store::Store;

/// Upper bound on the number of nodes built before pruning, protects the
/// server from a query that would otherwise allocate unbounded memory.
const MAX_NODES: usize = 400_000;

/// A flamegraph in the compact "names / levels / values" form.
///
/// * `names[i]` — display name of node `i`
/// * `levels[i]` — index of the parent node, `-1` for the root
/// * `values[i]` — inclusive value of the node
///
/// Node 0 is always the root. Children are emitted before their siblings in
/// no particular order; the UI sorts by value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Flamegraph {
    pub names: Vec<String>,
    pub levels: Vec<i64>,
    pub values: Vec<i64>,
    /// Total value of the root node.
    pub total: i64,
    /// Unit of `values`.
    pub unit: String,
    /// Number of samples that contributed.
    pub samples: u64,
    /// True when the tree was truncated to `max_nodes`.
    pub truncated: bool,
}

impl Flamegraph {
    pub fn empty(unit: Unit) -> Self {
        Flamegraph {
            names: vec!["root".to_string()],
            levels: vec![-1],
            values: vec![0],
            total: 0,
            unit: unit.as_str().to_string(),
            samples: 0,
            truncated: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Flatten to a tree for the UI: `(name, value, children)`.
    pub fn to_tree(&self) -> Vec<TreeNode> {
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); self.names.len()];
        for (i, &p) in self.levels.iter().enumerate() {
            if p >= 0 {
                children[p as usize].push(i);
            }
        }
        for c in &mut children {
            c.sort_unstable_by(|a, b| self.values[*b].cmp(&self.values[*a]));
        }
        fn build(fg: &Flamegraph, idx: usize, children: &[Vec<usize>]) -> TreeNode {
            TreeNode {
                name: fg.names[idx].clone(),
                value: fg.values[idx],
                children: children[idx]
                    .iter()
                    .map(|&c| build(fg, c, children))
                    .collect(),
            }
        }
        if self.names.is_empty() {
            return Vec::new();
        }
        vec![build(self, 0, &children)]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    pub name: String,
    pub value: i64,
    pub children: Vec<TreeNode>,
}

/// Parameters shared by all query types.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub profile_type: ProfileType,
    pub from_ns: u64,
    pub until_ns: u64,
    pub selector: Selector,
    pub max_nodes: usize,
}

impl QueryOptions {
    pub fn new(
        profile_type: impl Into<ProfileType>,
        from_ns: u64,
        until_ns: u64,
        selector: Selector,
    ) -> Self {
        QueryOptions {
            profile_type: profile_type.into(),
            from_ns,
            until_ns,
            selector,
            max_nodes: 1024,
        }
    }

    pub fn with_max_nodes(mut self, max_nodes: usize) -> Self {
        self.max_nodes = max_nodes.clamp(16, 50_000);
        self
    }

    fn validate(&self) -> Result<()> {
        if self.until_ns <= self.from_ns {
            return Err(Error::InvalidRange {
                from: self.from_ns,
                until: self.until_ns,
            });
        }
        Ok(())
    }
}

/// Interns frames across segments so identical functions collapse.
struct FrameDict {
    frames: Vec<Frame>,
    index: HashMap<(String, String, String, u32), u32>,
}

impl FrameDict {
    fn new() -> Self {
        FrameDict {
            frames: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn intern(&mut self, frame: Frame) -> u32 {
        let key = (
            frame.module.clone(),
            frame.symbol.clone(),
            frame.file.clone(),
            frame.line,
        );
        if let Some(&id) = self.index.get(&key) {
            return id;
        }
        let id = self.frames.len() as u32;
        self.index.insert(key, id);
        self.frames.push(frame);
        id
    }

    fn name(&self, id: u32) -> &str {
        self.frames
            .get(id as usize)
            .map(Frame::display_name_ref)
            .unwrap_or("<unknown>")
    }
}

impl Frame {
    fn display_name_ref(&self) -> &str {
        if !self.symbol.is_empty() {
            &self.symbol
        } else if !self.module.is_empty() {
            &self.module
        } else {
            "<unknown>"
        }
    }
}

/// Resolve the frame ids of one segment into the global dictionary.
fn build_remap(seg: &Segment, dict: &mut FrameDict) -> Vec<u32> {
    let count = seg.frame_count();
    let mut remap = Vec::with_capacity(count);
    for id in 0..count as u32 {
        match seg.frame_at(id) {
            Some((m, s, f, line)) => {
                let mut frame = Frame::new(seg.string_at(m), seg.string_at(s));
                frame.file = seg.string_at(f).to_string();
                frame.line = line;
                remap.push(dict.intern(frame));
            }
            None => remap.push(u32::MAX),
        }
    }
    remap
}

/// Incrementally built flamegraph tree.
struct TreeBuilder {
    dict: FrameDict,
    /// frame id per node, `u32::MAX` for the synthetic truncation node
    node_frame: Vec<u32>,
    parent: Vec<i32>,
    self_value: Vec<u64>,
    children: Vec<HashMap<u32, usize>>,
    truncated_node: Option<usize>,
    truncated: bool,
}

impl TreeBuilder {
    fn new() -> Self {
        TreeBuilder {
            dict: FrameDict::new(),
            node_frame: vec![u32::MAX],
            parent: vec![-1],
            self_value: vec![0],
            children: vec![HashMap::new()],
            truncated_node: None,
            truncated: false,
        }
    }

    fn add_stack(&mut self, frames: &[u32], value: u64) {
        let mut node = 0usize;
        // Stacks are stored leaf-first; the tree is built root-first.
        for &frame in frames.iter().rev() {
            if frame == u32::MAX {
                continue;
            }
            let existing = self.children[node].get(&frame).copied();
            node = match existing {
                Some(child) => child,
                None => {
                    if self.node_frame.len() >= MAX_NODES {
                        self.truncated = true;
                        let idx = match self.truncated_node {
                            Some(idx) => idx,
                            None => {
                                let idx = self.node_frame.len();
                                self.node_frame.push(u32::MAX);
                                self.parent.push(0);
                                self.self_value.push(0);
                                self.children.push(HashMap::new());
                                self.children[0].insert(u32::MAX, idx);
                                self.truncated_node = Some(idx);
                                idx
                            }
                        };
                        idx
                    } else {
                        let idx = self.node_frame.len();
                        self.node_frame.push(frame);
                        self.parent.push(node as i32);
                        self.self_value.push(0);
                        self.children.push(HashMap::new());
                        self.children[node].insert(frame, idx);
                        idx
                    }
                }
            };
        }
        self.self_value[node] = self.self_value[node].saturating_add(value);
    }

    fn finish(self, max_nodes: usize) -> Flamegraph {
        // Inclusive values: children always have a larger index than their
        // parent, so one reverse pass is enough.
        let mut value = self.self_value.clone();
        for i in (1..value.len()).rev() {
            let p = self.parent[i];
            if p >= 0 {
                value[p as usize] = value[p as usize].saturating_add(value[i]);
            }
        }

        let total = value[0];
        // ---- prune -------------------------------------------------------
        let max_nodes = max_nodes.max(2);
        let mut alive = vec![true; value.len()];
        let mut alive_children = vec![0u32; value.len()];
        for i in 1..value.len() {
            alive_children[self.parent[i] as usize] += 1;
        }
        let mut alive_count = value.len();
        let mut heap = std::collections::BinaryHeap::new();
        for i in 1..value.len() {
            if alive_children[i] == 0 {
                heap.push(std::cmp::Reverse((value[i], i)));
            }
        }
        while alive_count > max_nodes {
            let Some(std::cmp::Reverse((v, idx))) = heap.pop() else {
                break;
            };
            if !alive[idx] {
                continue;
            }
            alive[idx] = false;
            alive_count -= 1;
            let p = self.parent[idx] as usize;
            alive_children[p] = alive_children[p].saturating_sub(1);
            value[p] = value[p].saturating_sub(v);
            if alive_children[p] == 0 && p != 0 {
                heap.push(std::cmp::Reverse((value[p], p)));
            }
        }
        let was_pruned = alive_count < value.len();

        // ---- emit --------------------------------------------------------
        let mut children_alive: Vec<Vec<usize>> = vec![Vec::new(); value.len()];
        for i in 1..value.len() {
            if alive[i] {
                children_alive[self.parent[i] as usize].push(i);
            }
        }

        let mut names = Vec::with_capacity(alive_count);
        let mut levels = Vec::with_capacity(alive_count);
        let mut values = Vec::with_capacity(alive_count);
        let mut index_map = vec![u32::MAX; value.len()];

        // Iterative DFS, children sorted by value descending.
        let mut stack: Vec<(usize, i64)> = vec![(0, -1)];
        while let Some((node, parent_new)) = stack.pop() {
            let new_idx = names.len();
            index_map[node] = new_idx as u32;
            let name = if node == 0 {
                "root".to_string()
            } else if self.node_frame[node] == u32::MAX {
                "(truncated)".to_string()
            } else {
                self.dict.name(self.node_frame[node]).to_string()
            };
            names.push(name);
            levels.push(parent_new);
            values.push(value[node] as i64);

            children_alive[node].sort_unstable_by(|a, b| value[*b].cmp(&value[*a]));
            for &child in children_alive[node].iter().rev() {
                stack.push((child, new_idx as i64));
            }
        }

        let samples = self.self_value.iter().skip(1).filter(|&&v| v > 0).count() as u64;
        Flamegraph {
            names,
            levels,
            values,
            total: total as i64,
            unit: Unit::Nanoseconds.as_str().to_string(),
            samples,
            truncated: was_pruned || self.truncated,
        }
    }
}

/// Build a flamegraph from every segment matching `opts`.
pub fn flamegraph(store: &Store, opts: &QueryOptions) -> Result<Flamegraph> {
    opts.validate()?;
    let metas = store.select(&opts.profile_type, opts.from_ns, opts.until_ns, &opts.selector);
    let mut builder = TreeBuilder::new();
    if metas.is_empty() {
        return Ok(Flamegraph::empty(opts.profile_type.unit()));
    }

    let mut stack_buf: Vec<u32> = Vec::with_capacity(160);
    for meta in &metas {
        let seg = match Segment::open(&meta.path) {
            Ok(seg) => seg,
            Err(err) => {
                warn!(path = %meta.path.display(), error = %err, "skipping unreadable segment");
                continue;
            }
        };
        let remap = build_remap(&seg, &mut builder.dict);
        for i in 0..seg.sample_count() {
            let Some((stack_id, value)) = seg.sample_at(i) else {
                continue;
            };
            stack_buf.clear();
            seg.for_each_frame_id(stack_id, |fid| {
                let mapped = remap.get(fid as usize).copied().unwrap_or(u32::MAX);
                stack_buf.push(mapped);
            });
            builder.add_stack(&stack_buf, value);
        }
    }

    let mut fg = builder.finish(opts.max_nodes);
    fg.unit = opts.profile_type.unit().as_str().to_string();
    Ok(fg)
}

/// One row of the "top functions" table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopEntry {
    pub name: String,
    pub module: String,
    pub file: String,
    pub line: u32,
    /// Value where this function is the leaf frame.
    pub self_value: i64,
    /// Value of every stack containing this function.
    pub total_value: i64,
}

/// Aggregate the most expensive functions, by self value.
pub fn top(store: &Store, opts: &QueryOptions, limit: usize) -> Result<Vec<TopEntry>> {
    opts.validate()?;
    let limit = limit.clamp(1, 1000);
    let metas = store.select(&opts.profile_type, opts.from_ns, opts.until_ns, &opts.selector);
    let mut dict = FrameDict::new();
    let mut self_values: Vec<u64> = Vec::new();
    let mut total_values: Vec<u64> = Vec::new();
    let mut stack_buf: Vec<u32> = Vec::with_capacity(160);

    for meta in &metas {
        let seg = match Segment::open(&meta.path) {
            Ok(seg) => seg,
            Err(err) => {
                warn!(path = %meta.path.display(), error = %err, "skipping unreadable segment");
                continue;
            }
        };
        let remap = build_remap(&seg, &mut dict);
        if self_values.len() < dict.frames.len() {
            self_values.resize(dict.frames.len(), 0);
            total_values.resize(dict.frames.len(), 0);
        }
        for i in 0..seg.sample_count() {
            let Some((stack_id, value)) = seg.sample_at(i) else {
                continue;
            };
            stack_buf.clear();
            seg.for_each_frame_id(stack_id, |fid| {
                stack_buf.push(remap.get(fid as usize).copied().unwrap_or(u32::MAX));
            });
            for (pos, &frame) in stack_buf.iter().enumerate() {
                if frame == u32::MAX {
                    continue;
                }
                let frame = frame as usize;
                if frame >= total_values.len() {
                    self_values.resize(frame + 1, 0);
                    total_values.resize(frame + 1, 0);
                }
                total_values[frame] = total_values[frame].saturating_add(value);
                if pos == 0 {
                    self_values[frame] = self_values[frame].saturating_add(value);
                }
            }
        }
    }

    let mut rows: Vec<TopEntry> = dict
        .frames
        .iter()
        .enumerate()
        .map(|(i, frame)| TopEntry {
            name: frame.display_name(),
            module: frame.module.clone(),
            file: frame.file.clone(),
            line: frame.line,
            self_value: self_values.get(i).copied().unwrap_or(0) as i64,
            total_value: total_values.get(i).copied().unwrap_or(0) as i64,
        })
        .filter(|row| row.total_value > 0)
        .collect();
    rows.sort_unstable_by(|a, b| {
        b.self_value
            .cmp(&a.self_value)
            .then_with(|| b.total_value.cmp(&a.total_value))
            .then_with(|| a.name.cmp(&b.name))
    });
    rows.truncate(limit);
    Ok(rows)
}

/// Parameters for [`timeseries`].
#[derive(Debug, Clone)]
pub struct TimeSeriesOptions {
    pub profile_type: ProfileType,
    pub from_ns: u64,
    pub until_ns: u64,
    pub selector: Selector,
    /// Width of a bucket in nanoseconds.
    pub step_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesPoints {
    pub service_name: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub points: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeSeries {
    pub from_ns: u64,
    pub until_ns: u64,
    pub step_ns: u64,
    pub series: Vec<SeriesPoints>,
}

/// Aggregate values into fixed-width buckets, splitting a segment's total
/// proportionally over the buckets it overlaps.
pub fn timeseries(store: &Store, opts: &TimeSeriesOptions) -> Result<TimeSeries> {
    if opts.until_ns <= opts.from_ns {
        return Err(Error::InvalidRange {
            from: opts.from_ns,
            until: opts.until_ns,
        });
    }
    let step_ns = opts.step_ns.max(1);
    let buckets = ((opts.until_ns - opts.from_ns) / step_ns).max(1) as usize;
    let buckets = buckets.min(10_000);

    let metas = store.select(&opts.profile_type, opts.from_ns, opts.until_ns, &opts.selector);
    let mut map: HashMap<(String, std::collections::BTreeMap<String, String>), SeriesPoints> =
        HashMap::new();

    for meta in &metas {
        let key = (meta.service_name.clone(), meta.labels.clone());
        let series = map.entry(key).or_insert_with(|| SeriesPoints {
            service_name: meta.service_name.clone(),
            labels: meta.labels.clone(),
            points: vec![0; buckets],
        });

        let seg_start = meta.start_ns.max(opts.from_ns);
        let seg_end = meta.end_ns.min(opts.until_ns);
        if seg_end <= seg_start {
            continue;
        }
        let seg_len = (seg_end - seg_start) as u128;
        let mut pos = seg_start;
        while pos < seg_end {
            let bucket = ((pos - opts.from_ns) / step_ns) as usize;
            if bucket >= buckets {
                break;
            }
            let bucket_end = opts.from_ns + (bucket as u64 + 1) * step_ns;
            let chunk_end = bucket_end.min(seg_end);
            let chunk = (chunk_end - pos) as u128;
            let weighted = (meta.total_value as u128 * chunk) / seg_len;
            series.points[bucket] = series.points[bucket].saturating_add(weighted as i64);
            pos = chunk_end;
        }
    }

    let mut series: Vec<SeriesPoints> = map.into_values().collect();
    series.sort_unstable_by(|a, b| {
        let sa: i64 = a.points.iter().sum();
        let sb: i64 = b.points.iter().sum();
        sb.cmp(&sa).then_with(|| a.service_name.cmp(&b.service_name))
    });

    Ok(TimeSeries {
        from_ns: opts.from_ns,
        until_ns: opts.until_ns,
        step_ns,
        series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReportBuilder;
    use crate::store::{Store, StoreConfig};

    fn store_with_data(name: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lidlesseye-q-{name}-{}", uuid::Uuid::new_v4()));
        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        let mut b = ReportBuilder::new("checkout", "cpu", 1_000, 2_000).label("pod", "p1");
        b.add_stack(
            [
                Frame::new("/bin/app", "handle"),
                Frame::new("/bin/app", "serve"),
                Frame::new("/bin/app", "main"),
            ],
            500,
        );
        b.add_stack(
            [
                Frame::new("/bin/app", "encode"),
                Frame::new("/bin/app", "serve"),
                Frame::new("/bin/app", "main"),
            ],
            300,
        );
        b.add_stack(
            [
                Frame::new("/bin/app", "idle"),
                Frame::new("/bin/app", "main"),
            ],
            200,
        );
        store.append(&b.finish()).unwrap();
        (store, dir)
    }

    #[test]
    fn flamegraph_is_a_tree_with_inclusive_values() {
        let (store, dir) = store_with_data("fg");
        let opts = QueryOptions::new("cpu", 0, 10_000, Selector::default());
        let fg = flamegraph(&store, &opts).unwrap();
        assert_eq!(fg.total, 1000);
        assert_eq!(fg.names[0], "root");
        assert_eq!(fg.levels[0], -1);
        // main is an ancestor of everything and must sum to 1000
        let main_idx = fg.names.iter().position(|n| n == "main").unwrap();
        assert_eq!(fg.values[main_idx], 1000);
        // serve sums to 800
        let serve_idx = fg.names.iter().position(|n| n == "serve").unwrap();
        assert_eq!(fg.values[serve_idx], 800);
        // handle is a leaf with 500
        let handle_idx = fg.names.iter().position(|n| n == "handle").unwrap();
        assert_eq!(fg.values[handle_idx], 500);
        assert_eq!(fg.levels[handle_idx], serve_idx as i64);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn flamegraph_merges_segments_and_filters_labels() {
        let (store, dir) = store_with_data("merge");
        let mut b = ReportBuilder::new("checkout", "cpu", 3_000, 4_000).label("pod", "p2");
        b.add_stack(
            [
                Frame::new("/bin/app", "handle"),
                Frame::new("/bin/app", "serve"),
                Frame::new("/bin/app", "main"),
            ],
            100,
        );
        store.append(&b.finish()).unwrap();

        let all = flamegraph(
            &store,
            &QueryOptions::new("cpu", 0, 10_000, Selector::default()),
        )
        .unwrap();
        assert_eq!(all.total, 1100);

        let filtered = flamegraph(
            &store,
            &QueryOptions::new(
                "cpu",
                0,
                10_000,
                Selector::parse(r#"{pod="p2"}"#).unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(filtered.total, 100);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn top_sorts_by_self_value() {
        let (store, dir) = store_with_data("top");
        let rows = top(
            &store,
            &QueryOptions::new("cpu", 0, 10_000, Selector::default()),
            10,
        )
        .unwrap();
        assert_eq!(rows[0].name, "handle");
        assert_eq!(rows[0].self_value, 500);
        assert_eq!(rows[0].total_value, 500);
        let main = rows.iter().find(|r| r.name == "main").unwrap();
        assert_eq!(main.self_value, 0);
        assert_eq!(main.total_value, 1000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pruning_keeps_the_tree_bounded() {
        let (store, dir) = store_with_data("prune");
        let fg = flamegraph(
            &store,
            &QueryOptions::new("cpu", 0, 10_000, Selector::default()).with_max_nodes(16),
        )
        .unwrap();
        assert!(fg.names.len() <= 16);
        assert_eq!(fg.total, 1000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wall_merges_cpu_and_off_cpu() {
        let (store, dir) = store_with_data("wall");
        let mut b = ReportBuilder::new("checkout", "off_cpu", 1_000, 2_000);
        b.add_stack(
            [
                Frame::new("/bin/app", "wait_io"),
                Frame::new("/bin/app", "main"),
            ],
            4000,
        );
        store.append(&b.finish()).unwrap();

        let fg = flamegraph(
            &store,
            &QueryOptions::new("wall", 0, 10_000, Selector::default()),
        )
        .unwrap();
        assert_eq!(fg.total, 5000);
        let wait = fg.names.iter().position(|n| n == "wait_io").unwrap();
        assert_eq!(fg.values[wait], 4000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timeseries_distributes_segments() {
        let (store, dir) = store_with_data("ts");
        let ts = timeseries(
            &store,
            &TimeSeriesOptions {
                profile_type: ProfileType::cpu(),
                from_ns: 0,
                until_ns: 10_000,
                selector: Selector::default(),
                step_ns: 1_000,
            },
        )
        .unwrap();
        assert_eq!(ts.step_ns, 1_000);
        assert_eq!(ts.series.len(), 1);
        assert_eq!(ts.series[0].points.iter().sum::<i64>(), 1000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_store_returns_empty_flamegraph() {
        let dir = std::env::temp_dir().join(format!("lidlesseyeq-empty-{}", uuid::Uuid::new_v4()));
        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        let fg = flamegraph(
            &store,
            &QueryOptions::new("cpu", 0, 10, Selector::default()),
        )
        .unwrap();
        assert!(fg.is_empty());
        assert_eq!(fg.names, vec!["root"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
