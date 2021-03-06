//! Thread pool for blocking operations

use crate::loom::sync::{Arc, Condvar, Mutex};
use crate::loom::thread;
use crate::runtime::blocking::schedule::NoopSchedule;
use crate::runtime::blocking::shutdown;
use crate::runtime::blocking::task::BlockingTask;
use crate::runtime::builder::ThreadNameFn;
use crate::runtime::context;
use crate::runtime::task::{self, JoinHandle};
use crate::runtime::{Builder, Callback, Handle};

use slab::Slab;

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

pub(crate) struct BlockingPool {
    spawner: Spawner,
    shutdown_rx: shutdown::Receiver,
}

#[derive(Clone)]
pub(crate) struct Spawner {
    inner: Arc<Inner>,
}

struct Inner {
    /// State shared between worker threads
    shared: Mutex<Shared>,

    /// Pool threads wait on this.
    condvar: Condvar,

    /// Spawned threads use this name
    thread_name: ThreadNameFn,

    /// Spawned thread stack size
    stack_size: Option<usize>,

    /// Call after a thread starts
    after_start: Option<Callback>,

    /// Call before a thread stops
    before_stop: Option<Callback>,

    // Maximum number of threads
    thread_cap: usize,

    // Customizable wait timeout
    keep_alive: Duration,
}

struct Shared {
    queue: VecDeque<Task>,
    num_th: usize,
    num_idle: u32,
    num_notify: u32,
    shutdown: bool,
    shutdown_tx: Option<shutdown::Sender>,
    worker_threads: Slab<thread::JoinHandle<()>>,
}

type Task = task::Notified<NoopSchedule>;

const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// Run the provided function on an executor dedicated to blocking operations.
pub(crate) fn spawn_blocking<F, R>(func: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let rt = context::current().expect("not currently running on the Tokio runtime.");
    rt.spawn_blocking(func)
}

#[allow(dead_code)]
pub(crate) fn try_spawn_blocking<F, R>(func: F) -> Result<(), ()>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let rt = context::current().expect("not currently running on the Tokio runtime.");

    let (task, _handle) = task::joinable(BlockingTask::new(func));
    rt.blocking_spawner.spawn(task, &rt)
}

// ===== impl BlockingPool =====

impl BlockingPool {
    pub(crate) fn new(builder: &Builder, thread_cap: usize) -> BlockingPool {
        let (shutdown_tx, shutdown_rx) = shutdown::channel();
        let keep_alive = builder.keep_alive.unwrap_or(KEEP_ALIVE);

        BlockingPool {
            spawner: Spawner {
                inner: Arc::new(Inner {
                    shared: Mutex::new(Shared {
                        queue: VecDeque::new(),
                        num_th: 0,
                        num_idle: 0,
                        num_notify: 0,
                        shutdown: false,
                        shutdown_tx: Some(shutdown_tx),
                        worker_threads: Slab::new(),
                    }),
                    condvar: Condvar::new(),
                    thread_name: builder.thread_name.clone(),
                    stack_size: builder.thread_stack_size,
                    after_start: builder.after_start.clone(),
                    before_stop: builder.before_stop.clone(),
                    thread_cap,
                    keep_alive,
                }),
            },
            shutdown_rx,
        }
    }

    pub(crate) fn spawner(&self) -> &Spawner {
        &self.spawner
    }

    pub(crate) fn shutdown(&mut self, timeout: Option<Duration>) {
        let mut shared = self.spawner.inner.shared.lock();

        // The function can be called multiple times. First, by explicitly
        // calling `shutdown` then by the drop handler calling `shutdown`. This
        // prevents shutting down twice.
        if shared.shutdown {
            return;
        }

        shared.shutdown = true;
        shared.shutdown_tx = None;
        self.spawner.inner.condvar.notify_all();
        let mut workers = std::mem::replace(&mut shared.worker_threads, Slab::new());

        drop(shared);

        if self.shutdown_rx.wait(timeout) {
            for handle in workers.drain() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for BlockingPool {
    fn drop(&mut self) {
        self.shutdown(None);
    }
}

impl fmt::Debug for BlockingPool {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("BlockingPool").finish()
    }
}

// ===== impl Spawner =====

impl Spawner {
    pub(crate) fn spawn(&self, task: Task, rt: &Handle) -> Result<(), ()> {
        let shutdown_tx = {
            let mut shared = self.inner.shared.lock();

            if shared.shutdown {
                // Shutdown the task
                task.shutdown();

                // no need to even push this task; it would never get picked up
                return Err(());
            }

            shared.queue.push_back(task);

            if shared.num_idle == 0 {
                // No threads are able to process the task.

                if shared.num_th == self.inner.thread_cap {
                    // At max number of threads
                    None
                } else {
                    shared.num_th += 1;
                    assert!(shared.shutdown_tx.is_some());
                    shared.shutdown_tx.clone()
                }
            } else {
                // Notify an idle worker thread. The notification counter
                // is used to count the needed amount of notifications
                // exactly. Thread libraries may generate spurious
                // wakeups, this counter is used to keep us in a
                // consistent state.
                shared.num_idle -= 1;
                shared.num_notify += 1;
                self.inner.condvar.notify_one();
                None
            }
        };

        if let Some(shutdown_tx) = shutdown_tx {
            let mut shared = self.inner.shared.lock();
            let entry = shared.worker_threads.vacant_entry();

            let handle = self.spawn_thread(shutdown_tx, rt, entry.key());

            entry.insert(handle);
        }

        Ok(())
    }

    fn spawn_thread(
        &self,
        shutdown_tx: shutdown::Sender,
        rt: &Handle,
        worker_id: usize,
    ) -> thread::JoinHandle<()> {
        let mut builder = thread::Builder::new().name((self.inner.thread_name)());

        if let Some(stack_size) = self.inner.stack_size {
            builder = builder.stack_size(stack_size);
        }

        let rt = rt.clone();

        builder
            .spawn(move || {
                // Only the reference should be moved into the closure
                let _enter = crate::runtime::context::enter(rt.clone());
                rt.blocking_spawner.inner.run(worker_id);
                drop(shutdown_tx);
            })
            .unwrap()
    }
}

impl Inner {
    fn run(&self, worker_id: usize) {
        if let Some(f) = &self.after_start {
            f()
        }

        let mut shared = self.shared.lock();

        'main: loop {
            // BUSY
            while let Some(task) = shared.queue.pop_front() {
                drop(shared);
                task.run();

                shared = self.shared.lock();
            }

            // IDLE
            shared.num_idle += 1;

            while !shared.shutdown {
                let lock_result = self.condvar.wait_timeout(shared, self.keep_alive).unwrap();

                shared = lock_result.0;
                let timeout_result = lock_result.1;

                if shared.num_notify != 0 {
                    // We have received a legitimate wakeup,
                    // acknowledge it by decrementing the counter
                    // and transition to the BUSY state.
                    shared.num_notify -= 1;
                    break;
                }

                // Even if the condvar "timed out", if the pool is entering the
                // shutdown phase, we want to perform the cleanup logic.
                if !shared.shutdown && timeout_result.timed_out() {
                    shared.worker_threads.remove(worker_id);

                    break 'main;
                }

                // Spurious wakeup detected, go back to sleep.
            }

            if shared.shutdown {
                // Drain the queue
                while let Some(task) = shared.queue.pop_front() {
                    drop(shared);
                    task.shutdown();

                    shared = self.shared.lock();
                }

                // Work was produced, and we "took" it (by decrementing num_notify).
                // This means that num_idle was decremented once for our wakeup.
                // But, since we are exiting, we need to "undo" that, as we'll stay idle.
                shared.num_idle += 1;
                // NOTE: Technically we should also do num_notify++ and notify again,
                // but since we're shutting down anyway, that won't be necessary.
                break;
            }
        }

        // Thread exit
        shared.num_th -= 1;

        // num_idle should now be tracked exactly, panic
        // with a descriptive message if it is not the
        // case.
        shared.num_idle = shared
            .num_idle
            .checked_sub(1)
            .expect("num_idle underflowed on thread exit");

        if shared.shutdown && shared.num_th == 0 {
            self.condvar.notify_one();
        }

        drop(shared);

        if let Some(f) = &self.before_stop {
            f()
        }
    }
}

impl fmt::Debug for Spawner {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("blocking::Spawner").finish()
    }
}
