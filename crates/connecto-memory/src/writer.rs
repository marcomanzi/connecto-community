//! One writer thread per project (spec D3, ADR 0002). It owns the project's
//! [`Store`] (the only write connection) and runs jobs sent over a bounded
//! `crossbeam` channel, one at a time, in order. Readers open their own
//! [`crate::Reader`]s on their own threads; nothing here runs on the UI
//! thread.

use std::path::Path;
use std::thread::JoinHandle;

use crossbeam_channel::{Sender, TrySendError, bounded};

use crate::error::{Result, StoreError};
use crate::store::Store;

type Job = Box<dyn FnOnce(&mut Store) + Send>;

enum Msg {
    Job(Job),
    /// Finish the jobs queued before this and end the thread.
    Stop,
}

/// Why a job could not be posted without waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PostError {
    #[error("the memory writer's queue is full")]
    Full,
    #[error("the memory writer has stopped")]
    Gone,
}

/// A handle to send jobs to a writer; cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct WriterHandle {
    tx: Sender<Msg>,
}

/// Run `job` on the writer and flatten the writer's error with the job's.
pub(crate) fn call<R: Send + 'static>(
    writer: &WriterHandle,
    job: impl FnOnce(&mut Store) -> Result<R> + Send + 'static,
) -> Result<R> {
    writer.call(job)?
}

impl WriterHandle {
    /// Run `job` on the writer and wait for its result. Blocks while the
    /// queue is full and while earlier jobs run: never call it from the UI
    /// thread.
    pub fn call<R, F>(&self, job: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Store) -> R + Send + 'static,
    {
        let (reply_tx, reply_rx) = bounded(1);
        self.tx
            .send(Msg::Job(Box::new(move |store: &mut Store| {
                let _ = reply_tx.send(job(store));
            })))
            .map_err(|_| StoreError::WriterGone)?;
        reply_rx.recv().map_err(|_| StoreError::WriterGone)
    }

    /// Queue `job` without waiting, or say why not. For senders that must
    /// never block (event capture: a full queue is counted, not waited on).
    pub fn post<F>(&self, job: F) -> Result<(), PostError>
    where
        F: FnOnce(&mut Store) + Send + 'static,
    {
        self.tx
            .try_send(Msg::Job(Box::new(job)))
            .map_err(|e| match e {
                TrySendError::Full(_) => PostError::Full,
                TrySendError::Disconnected(_) => PostError::Gone,
            })
    }
}

/// The writer thread of one project. Dropping it lets the jobs already
/// queued finish, then ends and joins the thread; handles still around get
/// [`StoreError::WriterGone`] afterwards.
pub struct Writer {
    handle: WriterHandle,
    thread: Option<JoinHandle<()>>,
}

/// Default queue length.
pub const WRITER_QUEUE: usize = 256;

impl Writer {
    /// Open the store at `path` for `project_id` on a new thread. Returns
    /// once the database is open, or with the reason it could not be.
    pub fn spawn(path: &Path, project_id: &str, capacity: usize) -> Result<Self> {
        let (tx, rx) = bounded::<Msg>(capacity.max(1));
        let (ready_tx, ready_rx) = bounded::<Result<()>>(1);
        let path = path.to_owned();
        let project_id = project_id.to_owned();
        let thread = std::thread::Builder::new()
            .name(format!("memory-writer-{project_id}"))
            .spawn(move || {
                let mut store = match Store::open(&path, &project_id) {
                    Ok(store) => {
                        let _ = ready_tx.send(Ok(()));
                        store
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(Msg::Job(job)) = rx.recv() {
                    job(&mut store);
                }
            })?;
        let opened = ready_rx.recv().map_err(|_| StoreError::WriterGone)?;
        if let Err(e) = opened {
            let _ = thread.join();
            return Err(e);
        }
        Ok(Self {
            handle: WriterHandle { tx },
            thread: Some(thread),
        })
    }

    /// A handle for other threads.
    #[must_use]
    pub fn handle(&self) -> WriterHandle {
        self.handle.clone()
    }

    /// See [`WriterHandle::call`].
    pub fn call<R, F>(&self, job: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Store) -> R + Send + 'static,
    {
        self.handle.call(job)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(Msg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
