//! Which replica is allowed to write.
//!
//! Two replicas over the same data directory must not both flush, compact,
//! prune or archive: they would race on the same Parquet files and the same
//! WAL, which is exactly the class of bug that cost a week of data before.
//! Rather than invent a distributed consensus, this uses the one thing both
//! replicas already share — the data directory — for a lease file:
//!
//! * the holder writes `{holder, fence, expires_ns}` and refreshes it every
//!   few seconds;
//! * a replica that finds a live lease held by someone else is a standby: it
//!   serves reads and refuses writes;
//! * `fence` increases on every handover, so a resumed old holder can be told
//!   apart from the new one even if its clock drifted.
//!
//! The lease is deliberately conservative: a replica that cannot read or write
//! the file at all treats itself as a standby. Serving stale-reads is
//! recoverable; two writers are not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use lidlesseye_core::now_ns;

/// How long a lease stays valid without a refresh. Three ticks of [`TICK`] give
/// a holder two chances to be slow before it loses the role.
pub const TTL_NS: u64 = 10 * 1_000_000_000;

/// How often the holder refreshes and a standby looks for an opening.
pub const TICK: std::time::Duration = std::time::Duration::from_secs(3);

pub const FILE_NAME: &str = "active.lease";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub holder: String,
    pub fence: u64,
    pub expires_ns: u64,
    /// Where the holder accepts writes (`ip:port`). A standby hands writers a
    /// redirect to this address instead of failing them, so a Service that
    /// balances across both replicas still delivers every batch to the writer.
    #[serde(default)]
    pub addr: String,
}

/// The role this replica currently believes it has.
pub struct Lease {
    path: PathBuf,
    holder: String,
    addr: String,
    active: AtomicBool,
    fence: AtomicU64,
    /// Serialises this process's own refresh with its own reads.
    io: Mutex<()>,
    /// Address of the replica that currently holds the lease.
    addr_of: Mutex<String>,
    /// Set by [`Lease::passive`]: never read or write the lease file.
    passive: AtomicBool,
}

impl Lease {
    /// A replica that will never write: it does not read the lease file, it does
    /// not touch it, and it never becomes the holder.
    ///
    /// This exists because a read-only replica that still competed for the lease
    /// would be worse than useless: it could take the lease, keep renewing it,
    /// and then serve every write as a redirect to itself. Being passive is the
    /// only honest behaviour for a node that has decided not to write.
    pub fn passive(holder: impl Into<String>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            path: PathBuf::new(),
            holder: holder.into(),
            addr: String::new(),
            active: AtomicBool::new(false),
            fence: AtomicU64::new(0),
            io: Mutex::new(()),
            addr_of: Mutex::new(String::new()),
            passive: AtomicBool::new(true),
        })
    }

    #[must_use]
    pub fn is_passive(&self) -> bool {
        self.passive.load(Ordering::Acquire)
    }

    pub fn open(data_dir: &Path, holder: impl Into<String>) -> std::sync::Arc<Self> {
        Self::open_with_addr(data_dir, holder, String::new())
    }

    pub fn open_with_addr(
        data_dir: &Path,
        holder: impl Into<String>,
        addr: impl Into<String>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            path: data_dir.join(FILE_NAME),
            holder: holder.into(),
            addr: addr.into(),
            active: AtomicBool::new(false),
            fence: AtomicU64::new(0),
            io: Mutex::new(()),
            addr_of: Mutex::new(String::new()),
            passive: AtomicBool::new(false),
        })
    }

    /// `ip:port` of the current writer, empty when the replica did not announce
    /// one (older record or an unconfigured single process).
    pub fn active_addr(&self) -> String {
        self.addr_of.lock().clone()
    }

    /// True when this replica may write. A single unconfigured replica is
    /// always active, which keeps the default deployment a single writer
    /// without the lease file doing anything surprising.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn fence(&self) -> u64 {
        self.fence.load(Ordering::Relaxed)
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Refresh or claim. Returns true when the role changed (either way), so the
    /// caller can log a transition rather than every tick.
    pub fn tick(&self) -> bool {
        if self.passive.load(Ordering::Acquire) {
            return false;
        }
        let _guard = self.io.lock();
        let now = now_ns();
        let current = read(&self.path);
        let mine = current.as_ref().is_some_and(|r| r.holder == self.holder);
        let live = current
            .as_ref()
            .is_some_and(|r| r.expires_ns > now && !mine);
        if live {
            // A standby has to report the fence of the replica that holds the
            // lease, not its own last claim: the number is how an operator tells
            // "never was the writer" from "was, and was replaced".
            if let Some(record) = current {
                self.fence.store(record.fence, Ordering::Relaxed);
                *self.addr_of.lock() = record.addr.clone();
            }
            let was = self.active.swap(false, Ordering::Relaxed);
            return was;
        }
        // Free, expired, unreadable or already mine: take or keep it. The fence
        // only moves when the holder changes hands, so a steady holder keeps a
        // stable token and a takeover is visible to everyone.
        let fence = if mine {
            self.fence.load(Ordering::Relaxed)
        } else {
            current.map(|r| r.fence).unwrap_or(0) + 1
        };
        let record = LeaseRecord {
            holder: self.holder.clone(),
            fence,
            expires_ns: now + TTL_NS,
            addr: self.addr.clone(),
        };
        *self.addr_of.lock() = self.addr.clone();
        let was = self.active.swap(true, Ordering::Relaxed);
        self.fence.store(fence, Ordering::Relaxed);
        if let Err(err) = write(&self.path, &record) {
            // Losing the ability to prove ownership mid-flight is a reason to
            // stop writing, not to keep going on hope.
            tracing::warn!(error = %err, path = %self.path.display(), "lease: cannot write the lease file; standing down");
            self.active.store(false, Ordering::Relaxed);
            return was;
        }
        !was
    }
}

fn read(path: &Path) -> Option<LeaseRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write through a temporary file and rename, so a standby never reads a half
/// written lease and concludes the holder is gone.
fn write(path: &Path, record: &LeaseRecord) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(record)?)?;
    std::fs::rename(&tmp, path)
}
