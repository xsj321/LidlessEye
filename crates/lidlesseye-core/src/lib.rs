//! Core building blocks of LidlessEye: the profile model, the `LESEG`
//! columnar segment format, the on-disk store and the query engine.
//!
//! ```no_run
//! use lidlesseye_core::{Frame, ProfileType, QueryOptions, ReportBuilder, Selector, Store, StoreConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = Store::open(StoreConfig::new("./data"))?;
//!
//! let mut report = ReportBuilder::new("checkout", ProfileType::cpu(), 0, 10_000_000_000);
//! report.add_stack([Frame::new("/bin/checkout", "main")], 1_000_000);
//! store.append(&report.finish())?;
//!
//! let fg = lidlesseye_core::flamegraph(
//!     &store,
//!     &QueryOptions::new(ProfileType::cpu(), 0, 20_000_000_000, Selector::default()),
//! )?;
//! println!("{} nodes, total {}", fg.names.len(), fg.total);
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod model;
pub mod query;
pub mod segment;
pub mod selector;
pub mod store;
pub mod time;

pub use error::{Error, Result};
pub use model::{
    Frame, FrameRef, ProfileType, Report, ReportBuilder, StackSample, Unit, SERVICE_NAME_LABEL,
};
pub use query::{
    flamegraph, timeseries, top, Flamegraph, QueryOptions, SeriesPoints, TimeSeries,
    TimeSeriesOptions, TopEntry, TreeNode,
};
pub use segment::{Segment, SegmentMeta};
pub use selector::{glob_match, MatchOp, Matcher, Selector};
pub use store::{PruneStats, SeriesInfo, Store, StoreConfig, StoreStats};
pub use time::{now_ns, parse_duration_ns};
