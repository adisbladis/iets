//! Where the local store keeps an input source.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::error::{self, Error as CrateError};
use crate::session::Session;
use crate::source;
use crate::store::path::{StorePath, store_dir};
use crate::store::source::{SourceKind, SourceOrigin, SrcPath};

#[derive(Default)]
struct Slot {
    prefetched: OnceLock<Option<BTreeSet<StorePath>>>,
    answer: OnceLock<BTreeSet<StorePath>>,
}

/// What the store answered for a path, kept for the life of a session: the
/// references of a valid store path never change.
#[derive(Default)]
pub(crate) struct References {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    /// The queries [`prefetch_references`] started.
    started: AtomicUsize,
}

impl References {
    fn slot(&self, full: &str) -> (Arc<Slot>, bool) {
        let mut table = self.slots.lock().expect("references cache");
        match table.entry(full.to_string()) {
            Entry::Occupied(e) => (e.get().clone(), false),
            Entry::Vacant(e) => (e.insert(Arc::default()).clone(), true),
        }
    }
}

/// The references the local store holds for `path`, empty when it cannot
/// answer. primops.cc:addPath needs them for a store path it is given with a
/// context, and swallows `queryPathInfo`'s error the same way. The answer
/// is cached in `session`: a valid path's references never change.
pub fn references_of(session: &Session, path: &StorePath) -> BTreeSet<StorePath> {
    let full = path.to_string();
    let (s, _) = session.store_refs.slot(&full);
    // a failed prefetch queries again
    s.answer
        .get_or_init(|| {
            (s.prefetched.get_or_init(|| None).clone())
                .or_else(|| query_references(&full))
                .unwrap_or_default()
        })
        .clone()
}

const MAX_PREFETCH: usize = 8;

/// Starts `references_of(path)` on a thread.
pub fn prefetch_references(path: StorePath, session: Arc<Session>) {
    let refs = &session.store_refs;
    if refs.started.load(Ordering::Relaxed) >= MAX_PREFETCH {
        return;
    }
    let full = path.to_string();
    let (s, new) = refs.slot(&full);
    if new && refs.started.fetch_add(1, Ordering::Relaxed) < MAX_PREFETCH {
        thread::spawn(move || {
            let _entered = session.enter();
            s.prefetched.get_or_init(|| query_references(&full));
        });
    }
}

fn query_references(full: &str) -> Option<BTreeSet<StorePath>> {
    let info = source::local_daemon()?
        .query_path_info(full, &mut |_| {})
        .ok()??;
    info.references
        .iter()
        .map(|r| StorePath::parse(r).ok())
        .collect()
}

/// Answers the plan's locator from the local daemon: how each input source was
/// content-addressed and what it references. Paths named by `--source` are read from the given local tree instead and are plain sources. A path
/// that is not valid locally, or whose content address v1 cannot send (an input-addressed
/// output pinned with `builtins.storePath`), is `None`: the daemon substitutes it.
pub fn locate_in_store(
    paths: &[StorePath],
    overrides: &HashMap<String, PathBuf>,
) -> error::Result<Vec<Option<SourceOrigin>>> {
    let mut daemon = paths
        .iter()
        .any(|p| !overrides.contains_key(&p.to_string()))
        .then(source::local_daemon)
        .flatten();
    paths
        .iter()
        .map(|p| {
            let full = p.to_string();
            let unhashed = |at, kind| SourceOrigin {
                at,
                kind,
                nar: None,
            };
            if let Some(local) = overrides.get(&full) {
                return Ok(Some(unhashed(
                    SrcPath::fs(local.clone(), None),
                    SourceKind::Source,
                )));
            }
            let Some(d) = daemon.as_mut() else {
                return Ok(None);
            };
            let Some(info) = d.query_path_info(&full, &mut |_| {})? else {
                return Ok(None);
            };
            let kind = if info.ca.starts_with("fixed:r:sha256:") {
                SourceKind::Source
            } else if info.ca.starts_with("text:sha256:") {
                let references = info
                    .references
                    .iter()
                    .map(|r| StorePath::parse(r).map_err(CrateError::from))
                    .collect::<error::Result<_>>()?;
                SourceKind::Text { references }
            } else {
                return Ok(None);
            };
            Ok(Some(unhashed(
                SrcPath::fs(format!("{}/{}", store_dir(), p.base_name()), None),
                kind,
            )))
        })
        .collect()
}
