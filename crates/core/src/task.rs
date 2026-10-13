//! Plain-data task context shared by CLI, downloader and desktop workers.
use std::{
    cell::RefCell,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;
/// Cancellation and progress belong to a task, never to a native handle.
#[derive(Clone)]
pub struct TaskContext {
    /// Stable task identifier.
    pub id: Uuid,
    /// Cooperative cancellation flag.
    pub cancel: Arc<AtomicBool>,
    progress: Arc<dyn Fn(&str) + Send + Sync>,
}
impl Default for TaskContext {
    fn default() -> Self {
        Self::new(Uuid::new_v4(), Arc::new(AtomicBool::new(false)), |m| {
            eprintln!("{m}")
        })
    }
}
thread_local! { static CURRENT: RefCell<TaskContext> = RefCell::new(TaskContext::default()); }
/// Restores the previous thread context when dropped, including during unwinding.
pub struct TaskScope(Option<TaskContext>);
impl Drop for TaskScope {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.0.take().unwrap());
    }
}
impl TaskContext {
    /// Create an explicit task context. Callbacks must not panic.
    pub fn new(
        id: Uuid,
        cancel: Arc<AtomicBool>,
        progress: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            id,
            cancel,
            progress: Arc::new(progress),
        }
    }
    /// Current scoped context, with CLI defaults outside a scope.
    pub fn current() -> Self {
        CURRENT.with(|c| c.borrow().clone())
    }
    /// Report progress after checking cancellation.
    pub fn report(&self, message: impl AsRef<str>) -> io::Result<()> {
        self.check()?;
        (self.progress)(message.as_ref());
        Ok(())
    }
    /// Return Interrupted for cancellation, distinguishable from ordinary I/O failure.
    pub fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "task cancelled"))
        } else {
            Ok(())
        }
    }
    /// Install in a child worker thread; keep the guard alive for the task.
    pub fn install(&self) -> TaskScope {
        TaskScope(Some(CURRENT.with(|c| c.replace(self.clone()))))
    }
    /// Scope existing synchronous entry points; child workers explicitly inherit this clone.
    pub fn execute<T>(&self, body: impl FnOnce() -> T) -> T {
        let _scope = self.install();
        body()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_task_restores_after_unwind_and_inherits_cancellation() {
        let original = TaskContext::current().id;
        let flag = Arc::new(AtomicBool::new(false));
        let context = TaskContext::new(Uuid::new_v4(), flag.clone(), |_| {});
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            context.execute(|| {
                assert_eq!(TaskContext::current().id, context.id);
                panic!("injected")
            })
        }));
        assert!(result.is_err());
        assert_eq!(TaskContext::current().id, original);
        flag.store(true, Ordering::Relaxed);
        std::thread::spawn(move || {
            context.execute(|| {
                assert_eq!(
                    TaskContext::current().check().unwrap_err().kind(),
                    io::ErrorKind::Interrupted
                )
            })
        })
        .join()
        .unwrap();
    }
}
