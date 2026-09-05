use gpui::{PlatformDispatcher, Priority, RunnableVariant};
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Default)]
struct DispatcherState {
    runnables: VecDeque<RunnableVariant>,
    timers: Vec<(Instant, RunnableVariant)>,
}

/// A single-threaded scheduler that is pumped by the host through the `tick` export.
///
/// The guest never blocks: all work is queued locally, every guest turn drains the queues
/// (`run_until_idle`), and the turn reports the earliest remaining timer as the wakeup the
/// host should schedule. Nothing runs outside a turn, so no other wakeup path is needed.
pub struct PluginDispatcher {
    state: Mutex<DispatcherState>,
}

impl PluginDispatcher {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(DispatcherState::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DispatcherState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run every due timer and then every queued runnable, repeating until nothing remains.
    /// Runnables and timers may enqueue further work, so this loops until a pass does nothing.
    pub fn run_until_idle(&self) {
        loop {
            let now = Instant::now();
            let (due_timers, runnable) = {
                let mut state = self.lock();
                let mut due = Vec::new();
                let mut pending = Vec::new();
                for (deadline, runnable) in std::mem::take(&mut state.timers) {
                    if deadline <= now {
                        due.push(runnable);
                    } else {
                        pending.push((deadline, runnable));
                    }
                }
                state.timers = pending;
                let runnable = state.runnables.pop_front();
                (due, runnable)
            };

            let mut ran_any = false;
            for timer in due_timers {
                timer.run();
                ran_any = true;
            }
            if let Some(runnable) = runnable {
                runnable.run();
                ran_any = true;
            }
            if !ran_any {
                break;
            }
        }
    }

    /// The delay until the earliest pending timer, so the caller can schedule the next wakeup.
    pub fn next_timer_delay(&self) -> Option<Duration> {
        let state = self.lock();
        let now = Instant::now();
        state
            .timers
            .iter()
            .map(|(deadline, _)| deadline.saturating_duration_since(now))
            .min()
    }
}

impl PlatformDispatcher for PluginDispatcher {
    fn is_main_thread(&self) -> bool {
        true
    }

    fn dispatch(&self, runnable: RunnableVariant, _priority: Priority) {
        self.lock().runnables.push_back(runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        self.lock().runnables.push_back(runnable);
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let deadline = Instant::now() + duration;
        self.lock().timers.push((deadline, runnable));
    }

    fn spawn_realtime(&self, function: Box<dyn FnOnce() + Send>) {
        function();
    }
}
