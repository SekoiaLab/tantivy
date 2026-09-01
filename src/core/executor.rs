use std::sync::Arc;

#[cfg(feature = "quickwit")]
use futures_util::{future::Either, FutureExt};

use crate::TantivyError;

/// Tracks tasks throughout their lifecycle in the thread pool.
pub trait TaskInstrumentation: Send + Sync {
    /// Called when a task is added to the queue. Use guards in the
    /// EnqueuedTask implementation to track how long it stays there.
    fn enqueue(&self) -> Box<dyn EnqueuedTask>;

    /// Called when a task is scheduled in a scoped batch. Scheduling is
    /// different from the default spawn approach and we might want to track it
    /// separately.
    fn enqueue_scoped(&self) -> Box<dyn EnqueuedTask> {
        self.enqueue()
    }
}

/// Represents a task that has been enqueued but not yet executed. It is dropped
/// when the task starts running.
pub trait EnqueuedTask: Send {
    /// Called when the task starts running.
    fn run(self: Box<Self>) -> Box<dyn RunningTask>;

    /// Called when the task is scheduled in a scoped batch. Scheduling is
    /// different from the default spawn approach and we might want to track it
    /// separately.
    fn run_scoped(self: Box<Self>) -> Box<dyn RunningTask> {
        self.run()
    }
}

/// Represents a task that is currently running. It is dropped when the task
/// finishes.
pub trait RunningTask {}

/// Executor makes it possible to run tasks in single thread or
/// in a thread pool.
#[derive(Clone)]
pub enum Executor {
    /// Single thread variant of an Executor
    SingleThread,
    /// Thread pool variant of an Executor
    ThreadPool(Arc<rayon::ThreadPool>),
    /// Same as ThreadPool but calling instrumentation
    InstrumentedThreadPool(Arc<rayon::ThreadPool>, Arc<dyn TaskInstrumentation>),
}

#[cfg(feature = "quickwit")]
impl From<Arc<rayon::ThreadPool>> for Executor {
    fn from(thread_pool: Arc<rayon::ThreadPool>) -> Self {
        Executor::ThreadPool(thread_pool)
    }
}

impl Executor {
    /// Creates an Executor that performs all task in the caller thread.
    pub fn single_thread() -> Executor {
        Executor::SingleThread
    }

    /// Creates an Executor that dispatches the tasks in a thread pool.
    pub fn multi_thread(num_threads: usize, prefix: &'static str) -> crate::Result<Executor> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(move |num| format!("{prefix}{num}"))
            .build()?;
        Ok(Executor::ThreadPool(Arc::new(pool)))
    }

    /// Perform a map in the thread pool.
    ///
    /// Regardless of the executor (`SingleThread` or `ThreadPool`), panics in the task
    /// will propagate to the caller.
    pub fn map<A, R, F>(&self, f: F, args: impl Iterator<Item = A>) -> crate::Result<Vec<R>>
    where
        A: Send,
        R: Send,
        F: Sized + Sync + Fn(A) -> crate::Result<R>,
    {
        match self {
            Executor::SingleThread => {
                // Avoid `collect`, since the stacktrace is blown up by it, which makes profiling
                // harder.
                let mut result = Vec::with_capacity(args.size_hint().0);
                for arg in args {
                    result.push(f(arg)?);
                }
                Ok(result)
            }
            Executor::ThreadPool(pool) => {
                let args: Vec<A> = args.collect();
                let num_fruits = args.len();
                let fruit_receiver = {
                    let (fruit_sender, fruit_receiver) = crossbeam_channel::unbounded();
                    pool.scope(|scope| {
                        for (idx, arg) in args.into_iter().enumerate() {
                            // We name references for f and fruit_sender_ref because we do not
                            // want these two to be moved into the closure.
                            let f_ref = &f;
                            let fruit_sender_ref = &fruit_sender;
                            scope.spawn(move |_| {
                                let fruit = f_ref(arg);
                                if let Err(err) = fruit_sender_ref.send((idx, fruit)) {
                                    error!(
                                        "Failed to send search task. It probably means all search \
                                         threads have panicked. {err:?}"
                                    );
                                }
                            });
                        }
                    });
                    fruit_receiver
                    // This ends the scope of fruit_sender.
                    // This is important as it makes it possible for the fruit_receiver iteration to
                    // terminate.
                };
                let mut result_placeholders: Vec<Option<R>> =
                    std::iter::repeat_with(|| None).take(num_fruits).collect();
                for (pos, fruit_res) in fruit_receiver {
                    let fruit = fruit_res?;
                    result_placeholders[pos] = Some(fruit);
                }
                let results: Vec<R> = result_placeholders.into_iter().flatten().collect();
                if results.len() != num_fruits {
                    return Err(TantivyError::InternalError(
                        "One of the mapped execution failed.".to_string(),
                    ));
                }
                Ok(results)
            }
            Executor::InstrumentedThreadPool(pool, instrumentation) => {
                let args: Vec<(A, Box<dyn EnqueuedTask>)> = args
                    .map(|x| (x, instrumentation.enqueue_scoped()))
                    .collect();

                let num_fruits = args.len();
                let fruit_receiver = {
                    let (fruit_sender, fruit_receiver) = crossbeam_channel::unbounded();
                    pool.scope(|scope| {
                        for (idx, (arg, enqueued_task)) in args.into_iter().enumerate() {
                            // We name references for f and fruit_sender_ref because we do not
                            // want these two to be moved into the closure.
                            let f_ref = &f;
                            let fruit_sender_ref = &fruit_sender;
                            scope.spawn(move |_| {
                                let _running_task = enqueued_task.run_scoped();
                                let fruit = f_ref(arg);
                                if let Err(err) = fruit_sender_ref.send((idx, fruit)) {
                                    error!(
                                        "Failed to send search task. It probably means all search \
                                         threads have panicked. {err:?}"
                                    );
                                }
                            });
                        }
                    });
                    fruit_receiver
                    // This ends the scope of fruit_sender.
                    // This is important as it makes it possible for the fruit_receiver iteration to
                    // terminate.
                };
                let mut result_placeholders: Vec<Option<R>> =
                    std::iter::repeat_with(|| None).take(num_fruits).collect();
                for (pos, fruit_res) in fruit_receiver {
                    let fruit = fruit_res?;
                    result_placeholders[pos] = Some(fruit);
                }
                let results: Vec<R> = result_placeholders.into_iter().flatten().collect();
                if results.len() != num_fruits {
                    return Err(TantivyError::InternalError(
                        "One of the mapped execution failed.".to_string(),
                    ));
                }
                Ok(results)
            }
        }
    }

    /// Spawn a task on the pool, returning a future completing on task success.
    ///
    /// If the task panics, returns `Err(())`.
    #[cfg(feature = "quickwit")]
    pub fn spawn_blocking<T: Send + 'static>(
        &self,
        cpu_intensive_task: impl FnOnce() -> T + Send + 'static,
    ) -> impl std::future::Future<Output = Result<T, ()>> {
        match self {
            Executor::SingleThread => Either::Left(std::future::ready(Ok(cpu_intensive_task()))),
            Executor::ThreadPool(pool) => {
                let (sender, receiver) = oneshot::channel();
                pool.spawn(|| {
                    if sender.is_closed() {
                        return;
                    }
                    let task_result = cpu_intensive_task();
                    let _ = sender.send(task_result);
                });

                let res = receiver.map(|res| res.map_err(|_| ()));
                Either::Right(Either::Left(res))
            }
            Executor::InstrumentedThreadPool(pool, instrumentation) => {
                let enqueued_task = instrumentation.enqueue();
                let (sender, receiver) = oneshot::channel();
                pool.spawn(|| {
                    if sender.is_closed() {
                        return;
                    }
                    let _running_task = enqueued_task.run();
                    let task_result = cpu_intensive_task();
                    let _ = sender.send(task_result);
                });

                let res = receiver.map(|res| res.map_err(|_| ()));
                Either::Right(Either::Right(res))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::{EnqueuedTask, Executor, RunningTask, TaskInstrumentation};

    struct TestTaskInstrumentation {
        enqueue_count: Arc<AtomicUsize>,
        enqueue_scoped_count: Arc<AtomicUsize>,
        run_count: Arc<AtomicUsize>,
        run_scoped_count: Arc<AtomicUsize>,
    }
    struct TestEnqueuedTask {
        run_count: Arc<AtomicUsize>,
        run_scoped_count: Arc<AtomicUsize>,
    }
    struct TestRunningTask;

    impl TaskInstrumentation for TestTaskInstrumentation {
        fn enqueue(&self) -> Box<dyn EnqueuedTask> {
            self.enqueue_count.fetch_add(1, Ordering::Relaxed);
            Box::new(TestEnqueuedTask {
                run_count: Arc::clone(&self.run_count),
                run_scoped_count: Arc::clone(&self.run_scoped_count),
            })
        }

        fn enqueue_scoped(&self) -> Box<dyn EnqueuedTask> {
            self.enqueue_scoped_count.fetch_add(1, Ordering::Relaxed);
            Box::new(TestEnqueuedTask {
                run_count: Arc::clone(&self.run_count),
                run_scoped_count: Arc::clone(&self.run_scoped_count),
            })
        }
    }
    impl EnqueuedTask for TestEnqueuedTask {
        fn run(self: Box<Self>) -> Box<dyn RunningTask> {
            self.run_count.fetch_add(1, Ordering::Relaxed);
            Box::new(TestRunningTask)
        }

        fn run_scoped(self: Box<Self>) -> Box<dyn RunningTask> {
            self.run_scoped_count.fetch_add(1, Ordering::Relaxed);
            Box::new(TestRunningTask)
        }
    }
    impl RunningTask for TestRunningTask {}

    #[test]
    #[should_panic(expected = "panic should propagate")]
    fn test_panic_propagates_single_thread() {
        let _result: Vec<usize> = Executor::single_thread()
            .map(
                |_| {
                    panic!("panic should propagate");
                },
                vec![0].into_iter(),
            )
            .unwrap();
    }

    #[test]
    #[should_panic] //< unfortunately the panic message is not propagated
    fn test_panic_propagates_multi_thread() {
        let _result: Vec<usize> = Executor::multi_thread(1, "search-test")
            .unwrap()
            .map(
                |_| {
                    panic!("panic should propagate");
                },
                vec![0].into_iter(),
            )
            .unwrap();
    }

    #[test]
    fn test_map_singlethread() {
        let result: Vec<usize> = Executor::single_thread()
            .map(|i| Ok(i * 2), 0..1_000)
            .unwrap();
        assert_eq!(result.len(), 1_000);
        for i in 0..1_000 {
            assert_eq!(result[i], i * 2);
        }
    }

    #[test]
    fn test_map_multithread() {
        let result: Vec<usize> = Executor::multi_thread(3, "search-test")
            .unwrap()
            .map(|i| Ok(i * 2), 0..10)
            .unwrap();
        assert_eq!(result.len(), 10);
        for i in 0..10 {
            assert_eq!(result[i], i * 2);
        }
    }

    #[test]
    fn test_map_instrumented() {
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueue_scoped_count = Arc::new(AtomicUsize::new(0));
        let run_count = Arc::new(AtomicUsize::new(0));
        let run_scoped_count = Arc::new(AtomicUsize::new(0));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let executor = Executor::InstrumentedThreadPool(
            Arc::new(pool),
            Arc::new(TestTaskInstrumentation {
                enqueue_count: Arc::clone(&enqueue_count),
                enqueue_scoped_count: Arc::clone(&enqueue_scoped_count),
                run_count: Arc::clone(&run_count),
                run_scoped_count: Arc::clone(&run_scoped_count),
            }),
        );
        let result: Vec<usize> = executor.map(|i| Ok(i * 2), 0..10).unwrap();
        assert_eq!(result.len(), 10);
        for i in 0..10 {
            assert_eq!(result[i], i * 2);
        }
        assert_eq!(enqueue_count.load(Ordering::Relaxed), 0);
        assert_eq!(enqueue_scoped_count.load(Ordering::Relaxed), 10);
        assert_eq!(run_count.load(Ordering::Relaxed), 0);
        assert_eq!(run_scoped_count.load(Ordering::Relaxed), 10);
    }

    #[cfg(feature = "quickwit")]
    #[test]
    fn test_spawn_blocking_instrumented() {
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueue_scoped_count = Arc::new(AtomicUsize::new(0));
        let run_count = Arc::new(AtomicUsize::new(0));
        let run_scoped_count = Arc::new(AtomicUsize::new(0));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let executor = Executor::InstrumentedThreadPool(
            Arc::new(pool),
            Arc::new(TestTaskInstrumentation {
                enqueue_count: Arc::clone(&enqueue_count),
                enqueue_scoped_count: Arc::clone(&enqueue_scoped_count),
                run_count: Arc::clone(&run_count),
                run_scoped_count: Arc::clone(&run_scoped_count),
            }),
        );
        let result = futures::executor::block_on(executor.spawn_blocking(|| 42usize)).unwrap();
        assert_eq!(result, 42);
        assert_eq!(enqueue_count.load(Ordering::Relaxed), 1);
        assert_eq!(enqueue_scoped_count.load(Ordering::Relaxed), 0);
        assert_eq!(run_count.load(Ordering::Relaxed), 1);
        assert_eq!(run_scoped_count.load(Ordering::Relaxed), 0);
    }

    #[cfg(feature = "quickwit")]
    #[test]
    fn test_cancel_cpu_intensive_tasks() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let counter: Arc<AtomicU64> = Default::default();

        let other_counter: Arc<AtomicU64> = Default::default();

        let mut futures = Vec::new();
        let mut other_futures = Vec::new();

        let (tx, rx) = crossbeam_channel::bounded::<()>(0);
        let rx = Arc::new(rx);
        let executor = Executor::multi_thread(3, "search-test").unwrap();
        for _ in 0..1000 {
            let counter_clone: Arc<AtomicU64> = counter.clone();
            let other_counter_clone: Arc<AtomicU64> = other_counter.clone();

            let rx_clone = rx.clone();
            let rx_clone2 = rx.clone();
            let fut = executor.spawn_blocking(move || {
                counter_clone.fetch_add(1, Ordering::SeqCst);
                let _ = rx_clone.recv();
            });
            futures.push(fut);
            let other_fut = executor.spawn_blocking(move || {
                other_counter_clone.fetch_add(1, Ordering::SeqCst);
                let _ = rx_clone2.recv();
            });
            other_futures.push(other_fut);
        }

        // We execute 100 futures.
        for _ in 0..100 {
            tx.send(()).unwrap();
        }

        let counter_val = counter.load(Ordering::SeqCst);
        let other_counter_val = other_counter.load(Ordering::SeqCst);
        assert!(counter_val >= 30);
        assert!(other_counter_val >= 30);

        drop(other_futures);

        // We execute 100 futures.
        for _ in 0..100 {
            tx.send(()).unwrap();
        }

        let counter_val2 = counter.load(Ordering::SeqCst);
        assert!(counter_val2 >= counter_val + 100 - 6);

        let other_counter_val2 = other_counter.load(Ordering::SeqCst);
        assert!(other_counter_val2 <= other_counter_val + 6);
    }
}
