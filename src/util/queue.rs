//! A work queue of directories shared by the parallel tree walks: each worker takes a
//! directory, reads it, and queues the subdirectories it found, until no directory is pending
//! and none is being read.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use crate::error::{ForkspaceError, Result};

/// Visit `root` and every directory `visit` returns, on `threads` threads; `visit` reads one
/// directory and returns its subdirectories to visit next. The first error stops the walk.
pub(crate) fn run(
    root: &Path,
    threads: usize,
    visit: &(dyn Fn(&Path) -> Result<Vec<PathBuf>> + Sync),
) -> Result {
    let queue = Queue {
        state: Mutex::new(QueueState {
            pending: vec![root.to_path_buf()],
            busy: 0,
            failure: None,
        }),
        ready: Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| worker(&queue, visit));
        }
    });
    match queue.state.into_inner().expect("walk queue").failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    pending: Vec<PathBuf>,
    busy: usize,
    failure: Option<ForkspaceError>,
}

fn worker(queue: &Queue, visit: &(dyn Fn(&Path) -> Result<Vec<PathBuf>> + Sync)) {
    loop {
        let directory = {
            let mut state = queue.state.lock().expect("walk queue");
            loop {
                if state.failure.is_some() {
                    return;
                }
                if let Some(directory) = state.pending.pop() {
                    state.busy += 1;
                    break directory;
                }
                if state.busy == 0 {
                    queue.ready.notify_all();
                    return;
                }
                state = queue.ready.wait(state).expect("walk queue");
            }
        };
        let outcome = visit(&directory);
        let mut state = queue.state.lock().expect("walk queue");
        state.busy -= 1;
        match outcome {
            Ok(children) => state.pending.extend(children),
            Err(error) => {
                state.failure.get_or_insert(error);
            }
        }
        queue.ready.notify_all();
    }
}
