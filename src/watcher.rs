use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use globset::GlobMatcher;
use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::config::ResolvedPair;
use crate::gitignore::GitignoreCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncEventKind {
    CreateOrModify,
    Delete,
}

#[derive(Debug)]
pub struct SyncEvent {
    pub pair_index: usize,
    pub side: Side,
    pub kind: SyncEventKind,
    pub path: PathBuf,
    pub relative: PathBuf,
}

fn make_handler(
    pair_index: usize,
    side: Side,
    base: PathBuf,
    glob: GlobMatcher,
    gi_cache: Option<Arc<Mutex<GitignoreCache>>>,
    tx: mpsc::Sender<SyncEvent>,
) -> impl Fn(Result<Event, notify::Error>) + Send {
    move |res: Result<Event, notify::Error>| {
        let Ok(event) = res else { return };
        let kind = match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => SyncEventKind::CreateOrModify,
            EventKind::Remove(_) => SyncEventKind::Delete,
            _ => return,
        };
        for path in &event.paths {
            if !glob.is_match(path) {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&base) else {
                continue;
            };
            if let Some(ref cache) = gi_cache
                && cache.lock().unwrap().is_ignored(relative)
            {
                tracing::debug!("Skipping gitignored path: {}", path.display());
                continue;
            }
            let _ = tx.send(SyncEvent {
                pair_index,
                side,
                kind,
                path: path.clone(),
                relative: relative.to_path_buf(),
            });
        }
    }
}

/// Exposed for testing only.
pub fn setup_watchers(
    pairs: &[ResolvedPair],
    gi_caches: &[Option<Arc<Mutex<GitignoreCache>>>],
    tx: mpsc::Sender<SyncEvent>,
) -> Result<Vec<notify::RecommendedWatcher>, notify::Error> {
    let mut watchers = Vec::new();

    for (idx, pair) in pairs.iter().enumerate() {
        let gi_cache = gi_caches[idx].clone();

        // Watcher for side A
        let watcher_a = notify::recommended_watcher(make_handler(
            idx,
            Side::A,
            pair.a_base.clone(),
            pair.a_glob.clone(),
            gi_cache.clone(),
            tx.clone(),
        ))?;
        watchers.push(watcher_a);

        // Watcher for side B
        let watcher_b = notify::recommended_watcher(make_handler(
            idx,
            Side::B,
            pair.b_base.clone(),
            pair.b_glob.clone(),
            gi_cache,
            tx.clone(),
        ))?;
        watchers.push(watcher_b);
    }

    // Start watching after all watchers are created
    for (idx, pair) in pairs.iter().enumerate() {
        watchers[idx * 2].watch(&pair.a_base, RecursiveMode::Recursive)?;
        watchers[idx * 2 + 1].watch(&pair.b_base, RecursiveMode::Recursive)?;
    }

    Ok(watchers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::Glob;
    use notify::event::{AccessKind, CreateKind, RemoveKind};
    use tempfile::TempDir;

    fn call_handler(
        handler: &impl Fn(Result<Event, notify::Error>),
        kind: EventKind,
        path: PathBuf,
    ) {
        let mut event = Event::new(kind);
        event.paths.push(path);
        handler(Ok(event));
    }

    #[test]
    fn test_handler_sends_matching_event() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let pattern = format!("{}/**/*.md", base.display());
        let glob = Glob::new(&pattern).unwrap().compile_matcher();
        let (tx, rx) = mpsc::channel();

        let handler = make_handler(0, Side::A, base.clone(), glob, None, tx);
        let file_path = base.join("notes.md");

        call_handler(&handler, EventKind::Create(CreateKind::File), file_path);

        let event = rx.try_recv().unwrap();
        assert_eq!(event.pair_index, 0);
        assert_eq!(event.side, Side::A);
        assert_eq!(event.kind, SyncEventKind::CreateOrModify);
        assert_eq!(event.relative, PathBuf::from("notes.md"));
    }

    #[test]
    fn test_handler_ignores_non_matching() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let pattern = format!("{}/**/*.md", base.display());
        let glob = Glob::new(&pattern).unwrap().compile_matcher();
        let (tx, rx) = mpsc::channel();

        let handler = make_handler(0, Side::A, base.clone(), glob, None, tx);
        let file_path = base.join("data.txt");

        call_handler(&handler, EventKind::Create(CreateKind::File), file_path);

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_handler_maps_remove_to_delete() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let pattern = format!("{}/**/*.md", base.display());
        let glob = Glob::new(&pattern).unwrap().compile_matcher();
        let (tx, rx) = mpsc::channel();

        let handler = make_handler(0, Side::B, base.clone(), glob, None, tx);
        let file_path = base.join("removed.md");

        call_handler(&handler, EventKind::Remove(RemoveKind::File), file_path);

        let event = rx.try_recv().unwrap();
        assert_eq!(event.kind, SyncEventKind::Delete);
        assert_eq!(event.side, Side::B);
    }

    #[test]
    fn test_handler_ignores_access_events() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let pattern = format!("{}/**/*.md", base.display());
        let glob = Glob::new(&pattern).unwrap().compile_matcher();
        let (tx, rx) = mpsc::channel();

        let handler = make_handler(0, Side::A, base.clone(), glob, None, tx);
        let file_path = base.join("read.md");

        call_handler(&handler, EventKind::Access(AccessKind::Any), file_path);

        assert!(rx.try_recv().is_err());
    }
}
