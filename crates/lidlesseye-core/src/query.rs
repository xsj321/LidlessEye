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
    /// Optional label to split the profile on. Each distinct value becomes a
    /// frame directly under the root, which is how Pyroscope's "group by"
    /// renders.
    pub group_by: Option<String>,
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
            group_by: None,
        }
    }

    pub fn with_max_nodes(mut self, max_nodes: usize) -> Self {
        self.max_nodes = max_nodes.clamp(16, 50_000);
        self
    }

    pub fn with_group_by(mut self, label: Option<String>) -> Self {
        self.group_by = label.filter(|label| !label.trim().is_empty());
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
    /// Nodes whose display name is not derived from a frame (group-by nodes).
    overrides: HashMap<usize, String>,
    /// Group label value -> node directly under the root.
    group_nodes: HashMap<String, usize>,
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
            overrides: HashMap::new(),
            group_nodes: HashMap::new(),
        }
    }

    /// Node directly under the root holding the given group label value.
    fn group_node(&mut self, group: &str) -> usize {
        if let Some(&idx) = self.group_nodes.get(group) {
            return idx;
        }
        let idx = self.new_node(u32::MAX, 0);
        self.overrides.insert(idx, group.to_string());
        self.group_nodes.insert(group.to_string(), idx);
        idx
    }

    fn new_node(&mut self, frame: u32, parent: usize) -> usize {
        let idx = self.node_frame.len();
        self.node_frame.push(frame);
        self.parent.push(parent as i32);
        self.self_value.push(0);
        self.children.push(HashMap::new());
        if frame != u32::MAX {
            self.children[parent].insert(frame, idx);
        }
        idx
    }

    fn add_stack(&mut self, group: Option<&str>, frames: &[u32], value: u64) {
        let mut node = match group {
            Some(group) => self.group_node(group),
            None => 0usize,
        };
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
                        match self.truncated_node {
                            Some(idx) => idx,
                            None => {
                                let idx = self.new_node(u32::MAX, 0);
                                self.overrides.insert(idx, "(truncated)".to_string());
                                self.truncated_node = Some(idx);
                                idx
                            }
                        }
                    } else {
                        self.new_node(frame, node)
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
            } else if let Some(name) = self.overrides.get(&node) {
                name.clone()
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
    let metas = store.select(
        &opts.profile_type,
        opts.from_ns,
        opts.until_ns,
        &opts.selector,
    );
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
        let group = opts
            .group_by
            .as_deref()
            .and_then(|label| meta.label(label))
            .map(|value| value.to_string());
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
            builder.add_stack(group.as_deref(), &stack_buf, value);
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

/// The result of [`top`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopResult {
    pub items: Vec<TopEntry>,
    /// Value of every stack in the selection, i.e. the denominator for the
    /// "share of total" of each row. It is *not* the sum of `items`, which is
    /// truncated to the requested limit.
    pub total_value: i64,
}

/// Aggregate the most expensive functions, by self value.
pub fn top(store: &Store, opts: &QueryOptions, limit: usize) -> Result<TopResult> {
    opts.validate()?;
    let limit = limit.clamp(1, 1000);
    let metas = store.select(
        &opts.profile_type,
        opts.from_ns,
        opts.until_ns,
        &opts.selector,
    );
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

    let total_value: u64 = self_values.iter().sum();
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
    Ok(TopResult {
        items: rows,
        total_value: total_value as i64,
    })
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
    /// Split each service into one series per value of this label.
    pub group_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesPoints {
    pub service_name: String,
    pub labels: std::collections::BTreeMap<String, String>,
    /// Value of the group-by label, when the query asked for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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

    let metas = store.select(
        &opts.profile_type,
        opts.from_ns,
        opts.until_ns,
        &opts.selector,
    );
    type Key = (
        Option<String>,
        String,
        std::collections::BTreeMap<String, String>,
    );
    let mut map: HashMap<Key, SeriesPoints> = HashMap::new();

    for meta in &metas {
        let group = match &opts.group_by {
            Some(label) => meta.label(label).map(|value| value.to_string()),
            None => None,
        };
        let key: Key = match &opts.group_by {
            Some(_) => (
                group.clone(),
                String::new(),
                std::collections::BTreeMap::new(),
            ),
            None => (None, meta.service_name.clone(), meta.labels.clone()),
        };
        let grouped = opts.group_by.is_some();
        let series = map.entry(key).or_insert_with(|| SeriesPoints {
            service_name: if grouped {
                group.clone().unwrap_or_else(|| "(none)".to_string())
            } else {
                meta.service_name.clone()
            },
            labels: if grouped {
                std::collections::BTreeMap::new()
            } else {
                meta.labels.clone()
            },
            group,
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
        sb.cmp(&sa)
            .then_with(|| a.service_name.cmp(&b.service_name))
    });

    Ok(TimeSeries {
        from_ns: opts.from_ns,
        until_ns: opts.until_ns,
        step_ns,
        series,
    })
}

/// Options for a baseline-versus-comparison diff.
#[derive(Debug, Clone)]
pub struct DiffOptions {
    pub profile_type: ProfileType,
    /// Start of the "before" window.
    pub baseline_from_ns: u64,
    /// End of the "before" window.
    pub baseline_until_ns: u64,
    /// Start of the "after" window.
    pub from_ns: u64,
    /// End of the "after" window.
    pub until_ns: u64,
    pub selector: Selector,
    pub max_nodes: usize,
}

/// A flamegraph annotated with two windows, Pyroscope's "comparison view".
///
/// `left` is the baseline, `right` the comparison, and `diff` is
/// `right - left`, so a positive value is a regression and a negative value an
/// improvement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffFlamegraph {
    pub names: Vec<String>,
    pub levels: Vec<i64>,
    pub left: Vec<i64>,
    pub right: Vec<i64>,
    pub diff: Vec<i64>,
    pub total_left: i64,
    pub total_right: i64,
    pub unit: String,
    pub truncated: bool,
}

impl DiffFlamegraph {
    pub fn is_empty(&self) -> bool {
        self.total_left == 0 && self.total_right == 0
    }

    /// Relative change of the whole profile, in percent.
    pub fn total_ratio(&self) -> f64 {
        if self.total_left == 0 {
            if self.total_right == 0 {
                0.0
            } else {
                f64::INFINITY
            }
        } else {
            (self.total_right - self.total_left) as f64 / self.total_left as f64 * 100.0
        }
    }
}

struct DiffTree {
    dict: FrameDict,
    node_frame: Vec<u32>,
    parent: Vec<i32>,
    left_self: Vec<u64>,
    right_self: Vec<u64>,
    children: Vec<HashMap<u32, usize>>,
}

impl DiffTree {
    fn new() -> Self {
        DiffTree {
            dict: FrameDict::new(),
            node_frame: vec![u32::MAX],
            parent: vec![-1],
            left_self: vec![0],
            right_self: vec![0],
            children: vec![HashMap::new()],
        }
    }

    fn add_stack(&mut self, frames: &[u32], left: u64, right: u64) {
        let mut node = 0usize;
        for &frame in frames.iter().rev() {
            if frame == u32::MAX {
                continue;
            }
            let existing = self.children[node].get(&frame).copied();
            node = match existing {
                Some(child) => child,
                None => {
                    if self.node_frame.len() >= MAX_NODES {
                        // Stop growing the tree; the value still lands on the
                        // deepest node we already have, so totals stay exact.
                        break;
                    }
                    let idx = self.node_frame.len();
                    self.node_frame.push(frame);
                    self.parent.push(node as i32);
                    self.left_self.push(0);
                    self.right_self.push(0);
                    self.children.push(HashMap::new());
                    self.children[node].insert(frame, idx);
                    idx
                }
            };
        }
        self.left_self[node] = self.left_self[node].saturating_add(left);
        self.right_self[node] = self.right_self[node].saturating_add(right);
    }

    fn finish(self, max_nodes: usize) -> DiffFlamegraph {
        let mut left = self.left_self.clone();
        let mut right = self.right_self.clone();
        for i in (1..left.len()).rev() {
            let p = self.parent[i] as usize;
            left[p] = left[p].saturating_add(left[i]);
            right[p] = right[p].saturating_add(right[i]);
        }
        let total_left = left[0];
        let total_right = right[0];

        // Prune the smallest leaves until the tree fits, measuring importance
        // by the sum of both windows so a node that only exists on one side
        // still survives when it matters.
        let max_nodes = max_nodes.max(2);
        let mut alive = vec![true; left.len()];
        let mut alive_children = vec![0u32; left.len()];
        for i in 1..left.len() {
            alive_children[self.parent[i] as usize] += 1;
        }
        let mut alive_count = left.len();
        let mut heap = std::collections::BinaryHeap::new();
        for i in 1..left.len() {
            if alive_children[i] == 0 {
                heap.push(std::cmp::Reverse((left[i].saturating_add(right[i]), i)));
            }
        }
        while alive_count > max_nodes {
            let Some(std::cmp::Reverse((_, idx))) = heap.pop() else {
                break;
            };
            if !alive[idx] {
                continue;
            }
            alive[idx] = false;
            alive_count -= 1;
            let p = self.parent[idx] as usize;
            alive_children[p] = alive_children[p].saturating_sub(1);
            if alive_children[p] == 0 && p != 0 {
                heap.push(std::cmp::Reverse((left[p].saturating_add(right[p]), p)));
            }
        }

        let mut children_alive: Vec<Vec<usize>> = vec![Vec::new(); left.len()];
        for i in 1..left.len() {
            if alive[i] {
                children_alive[self.parent[i] as usize].push(i);
            }
        }
        let mut names = Vec::with_capacity(alive_count);
        let mut levels = Vec::with_capacity(alive_count);
        let mut left_out = Vec::with_capacity(alive_count);
        let mut right_out = Vec::with_capacity(alive_count);
        let mut stack: Vec<(usize, i64)> = vec![(0, -1)];
        while let Some((node, parent_new)) = stack.pop() {
            let new_idx = names.len();
            names.push(if node == 0 {
                "root".to_string()
            } else {
                self.dict.name(self.node_frame[node]).to_string()
            });
            levels.push(parent_new);
            left_out.push(left[node] as i64);
            right_out.push(right[node] as i64);
            children_alive[node]
                .sort_unstable_by(|a, b| (left[*b] + right[*b]).cmp(&(left[*a] + right[*a])));
            for &child in children_alive[node].iter().rev() {
                stack.push((child, new_idx as i64));
            }
        }
        let diff = left_out
            .iter()
            .zip(right_out.iter())
            .map(|(l, r)| r.saturating_sub(*l))
            .collect();
        DiffFlamegraph {
            names,
            levels,
            left: left_out,
            right: right_out,
            diff,
            total_left: total_left as i64,
            total_right: total_right as i64,
            unit: Unit::Nanoseconds.as_str().to_string(),
            truncated: self.left_self.len() > alive_count,
        }
    }
}

/// Compare two time windows and return the merged, annotated flamegraph.
pub fn diff(store: &Store, opts: &DiffOptions) -> Result<DiffFlamegraph> {
    if opts.until_ns <= opts.from_ns {
        return Err(Error::InvalidRange {
            from: opts.from_ns,
            until: opts.until_ns,
        });
    }
    if opts.baseline_until_ns <= opts.baseline_from_ns {
        return Err(Error::InvalidRange {
            from: opts.baseline_from_ns,
            until: opts.baseline_until_ns,
        });
    }
    let baseline = store.select(
        &opts.profile_type,
        opts.baseline_from_ns,
        opts.baseline_until_ns,
        &opts.selector,
    );
    let comparison = store.select(
        &opts.profile_type,
        opts.from_ns,
        opts.until_ns,
        &opts.selector,
    );

    let mut tree = DiffTree::new();
    let mut stack_buf: Vec<u32> = Vec::with_capacity(160);
    for (metas, is_baseline) in [(baseline, true), (comparison, false)] {
        for meta in &metas {
            let seg = match Segment::open(&meta.path) {
                Ok(seg) => seg,
                Err(err) => {
                    warn!(path = %meta.path.display(), error = %err, "skipping unreadable segment");
                    continue;
                }
            };
            let remap = build_remap(&seg, &mut tree.dict);
            for i in 0..seg.sample_count() {
                let Some((stack_id, value)) = seg.sample_at(i) else {
                    continue;
                };
                stack_buf.clear();
                seg.for_each_frame_id(stack_id, |fid| {
                    stack_buf.push(remap.get(fid as usize).copied().unwrap_or(u32::MAX));
                });
                let (left, right) = if is_baseline { (value, 0) } else { (0, value) };
                tree.add_stack(&stack_buf, left, right);
            }
        }
    }

    let mut result = tree.finish(opts.max_nodes);
    result.unit = opts.profile_type.unit().as_str().to_string();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReportBuilder;
    use crate::store::{Store, StoreConfig};

    fn store_with_data(name: &str) -> (Store, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-q-{name}-{}", uuid::Uuid::new_v4()));
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
            &QueryOptions::new("cpu", 0, 10_000, Selector::parse(r#"{pod="p2"}"#).unwrap()),
        )
        .unwrap();
        assert_eq!(filtered.total, 100);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn top_sorts_by_self_value() {
        let (store, dir) = store_with_data("top");
        let result = top(
            &store,
            &QueryOptions::new("cpu", 0, 10_000, Selector::default()),
            10,
        )
        .unwrap();
        let rows = &result.items;
        assert_eq!(result.total_value, 1000);
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
                group_by: None,
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

    #[test]
    fn group_by_adds_a_frame_per_label_value() {
        let (store, dir) = store_with_data("group");
        let mut b = ReportBuilder::new("checkout", "cpu", 3_000, 4_000).label("pod", "p2");
        b.add_stack([Frame::new("/bin/app", "main")], 250);
        store.append(&b.finish()).unwrap();

        let fg = flamegraph(
            &store,
            &QueryOptions::new("cpu", 0, 10_000, Selector::default())
                .with_group_by(Some("pod".to_string())),
        )
        .unwrap();
        assert_eq!(fg.total, 1250);
        // Root's children are the two pods.
        let groups: Vec<&str> = fg
            .levels
            .iter()
            .enumerate()
            .filter(|(_, level)| **level == 0)
            .map(|(i, _)| fg.names[i].as_str())
            .collect();
        assert_eq!(groups.len(), 2);
        assert!(groups.contains(&"p1"));
        assert!(groups.contains(&"p2"));

        // service_name works as a group-by key too.
        let by_service = flamegraph(
            &store,
            &QueryOptions::new("cpu", 0, 10_000, Selector::default())
                .with_group_by(Some("service_name".to_string())),
        )
        .unwrap();
        assert_eq!(
            by_service
                .levels
                .iter()
                .filter(|level| **level == 0)
                .count(),
            1
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn diff_reports_both_windows() {
        let (store, dir) = store_with_data("diff");
        // A second window where handle got twice as expensive and idle halved.
        let mut b = ReportBuilder::new("checkout", "cpu", 5_000, 6_000).label("pod", "p1");
        b.add_stack(
            [
                Frame::new("/bin/app", "handle"),
                Frame::new("/bin/app", "serve"),
                Frame::new("/bin/app", "main"),
            ],
            1_000,
        );
        b.add_stack(
            [
                Frame::new("/bin/app", "idle"),
                Frame::new("/bin/app", "main"),
            ],
            100,
        );
        store.append(&b.finish()).unwrap();

        let result = diff(
            &store,
            &DiffOptions {
                profile_type: ProfileType::cpu(),
                baseline_from_ns: 0,
                baseline_until_ns: 2_000,
                from_ns: 4_000,
                until_ns: 7_000,
                selector: Selector::default(),
                max_nodes: 1000,
            },
        )
        .unwrap();
        assert_eq!(result.total_left, 1000);
        assert_eq!(result.total_right, 1100);
        assert_eq!(result.diff[0], 100);
        let handle = result.names.iter().position(|n| n == "handle").unwrap();
        assert_eq!(result.left[handle], 500);
        assert_eq!(result.right[handle], 1000);
        assert_eq!(result.diff[handle], 500);
        let idle = result.names.iter().position(|n| n == "idle").unwrap();
        assert_eq!(result.diff[idle], -100);
        assert!((result.total_ratio() - 10.0).abs() < 1e-9);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timeseries_group_by_splits_series() {
        let (store, dir) = store_with_data("ts-group");
        let mut b = ReportBuilder::new("checkout", "cpu", 3_000, 4_000).label("pod", "p2");
        b.add_stack([Frame::new("/bin/app", "main")], 100);
        store.append(&b.finish()).unwrap();

        let ts = timeseries(
            &store,
            &TimeSeriesOptions {
                profile_type: ProfileType::cpu(),
                from_ns: 0,
                until_ns: 10_000,
                selector: Selector::default(),
                step_ns: 1_000,
                group_by: Some("pod".to_string()),
            },
        )
        .unwrap();
        assert_eq!(ts.series.len(), 2);
        let groups: Vec<&str> = ts
            .series
            .iter()
            .map(|s| s.group.as_deref().unwrap_or(""))
            .collect();
        assert!(groups.contains(&"p1"));
        assert!(groups.contains(&"p2"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
