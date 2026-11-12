//! Channel-to-entity bridge: backend threads push events, the UI applies them in batches.
//!
//! Backends (tokio tasks or plain threads) hold a cloneable [`BridgeSender`]. The UI side
//! calls [`attach`] once with a GPUI entity and a window; from then on
//!
//! - the channel is **bounded**, so a producer that outruns the UI is slowed down
//!   (`send().await` waits, `try_send` reports `Full`) instead of growing memory;
//! - the receiver sleeps while the channel is empty (no polling, no idle frames);
//! - once an event arrives it is drained in a **next-frame callback**, so events are applied
//!   **at most once per frame**: everything that queued up meanwhile (up to the channel
//!   capacity) is handed to `apply` as one ordered `Vec`, followed by **one**
//!   `cx.notify()` for the whole batch.
//!
//! A burst of 1 000 events therefore costs one `apply`, one notify and one layout pass. This
//! is the ingest-batching foundation for the firehose source (PR 1.3) and chat view (PR 1.4);
//! it is generic over the event type and the entity.
//!
//! Frame callbacks only run while the window is producing frames. If the window is hidden or
//! occluded the queue fills up and senders wait; that is the intended backpressure. When the
//! window or the entity is gone the attached task ends and the receiver is dropped, so
//! senders see `Closed`.

use gpui_kit::{AnyWindowHandle, App, AppContext as _, Context, Entity, Task};
use tokio::sync::{mpsc, oneshot};

pub use mpsc::error::{SendError, TrySendError};

/// Creates a bounded bridge channel. `capacity` is both the queue length (backpressure
/// threshold) and the largest batch one frame will apply; it must be at least 1.
pub fn channel<E>(capacity: usize) -> (BridgeSender<E>, BridgeReceiver<E>) {
    let (tx, rx) = mpsc::channel(capacity);
    (
        BridgeSender { tx },
        BridgeReceiver {
            rx,
            max_batch: capacity,
        },
    )
}

/// Producer half. Cheap to clone; use from any thread.
#[derive(Debug)]
pub struct BridgeSender<E> {
    tx: mpsc::Sender<E>,
}

impl<E> Clone for BridgeSender<E> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

// The producer API is used by the backends that land in PR 1.2 / 1.3.
#[allow(dead_code)]
impl<E> BridgeSender<E> {
    /// Waits for room in the queue (backpressure). Errors with the event if the UI side is
    /// gone.
    pub async fn send(&self, event: E) -> Result<(), SendError<E>> {
        self.tx.send(event).await
    }

    /// Blocking variant for plain threads (never call it from inside a tokio task).
    pub fn blocking_send(&self, event: E) -> Result<(), SendError<E>> {
        self.tx.blocking_send(event)
    }

    /// Non-blocking: `Full` when the UI is behind, `Closed` when it is gone.
    pub fn try_send(&self, event: E) -> Result<(), TrySendError<E>> {
        self.tx.try_send(event)
    }

    /// The UI side was dropped; further sends fail.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Consumer half, consumed by [`attach`]. Its two operations are plain async/sync code with
/// no GPUI in them, which is what the unit tests exercise.
#[derive(Debug)]
pub struct BridgeReceiver<E> {
    rx: mpsc::Receiver<E>,
    max_batch: usize,
}

impl<E> BridgeReceiver<E> {
    /// Waits for the next event; `None` once every sender is gone and the queue is empty.
    pub(crate) async fn first(&mut self) -> Option<E> {
        self.rx.recv().await
    }

    /// `first` plus everything already queued (oldest first), capped at the batch size.
    fn drain(&mut self, first: E) -> Vec<E> {
        let mut batch = vec![first];
        while batch.len() < self.max_batch {
            match self.rx.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break,
            }
        }
        batch
    }
}

/// Connects `receiver` to `entity`: each frame that follows an event, `apply` gets every
/// queued event in order and the entity is notified once. Returns the task driving it; drop
/// or `detach` it. The task ends (dropping the receiver) when all senders are gone, the
/// window closes or the entity is released.
pub fn attach<E, T>(
    entity: &Entity<T>,
    window: AnyWindowHandle,
    receiver: BridgeReceiver<E>,
    cx: &mut App,
    apply: impl FnMut(&mut T, Vec<E>, &mut Context<T>) + 'static,
) -> Task<()>
where
    E: 'static,
    T: 'static,
{
    let entity = entity.downgrade();
    cx.spawn(async move |cx| {
        // The receiver and `apply` travel into the frame callback and come back through the
        // oneshot, so no shared mutable state is needed. If the window closes, the callback
        // is dropped unrun and the oneshot reports it.
        let mut state = Some((receiver, apply));
        while let Some((mut receiver, mut apply)) = state.take() {
            let Some(first) = receiver.first().await else {
                break;
            };
            let (done_tx, done_rx) = oneshot::channel();
            let entity = entity.clone();
            let scheduled = cx.update_window(window, move |_, window, _| {
                window.on_next_frame(move |_, cx| {
                    let batch = receiver.drain(first);
                    let alive = entity
                        .update(cx, |target, cx| {
                            apply(target, batch, cx);
                            cx.notify();
                        })
                        .is_ok();
                    let _ = done_tx.send(alive.then_some((receiver, apply)));
                });
            });
            if scheduled.is_err() {
                break;
            }
            state = done_rx.await.ok().flatten();
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use gpui_kit::{Context, IntoElement, Render, TestAppContext, Window, WindowHandle, div};

    use super::*;

    // ---- the non-GPUI core ----------------------------------------------------------------

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn drain_coalesces_everything_queued_in_order() {
        let (tx, mut rx) = channel::<u32>(16);
        for event in 0..10 {
            tx.try_send(event).unwrap();
        }
        let first = block_on(rx.first()).unwrap();
        assert_eq!(rx.drain(first), (0..10).collect::<Vec<_>>());
        // Nothing left: the next batch starts from the next event.
        tx.try_send(10).unwrap();
        let first = block_on(rx.first()).unwrap();
        assert_eq!(rx.drain(first), vec![10]);
    }

    #[test]
    fn batch_is_capped_and_the_rest_stays_queued_in_order() {
        let (tx, mut rx) = channel::<u32>(4);
        for event in 0..4 {
            tx.try_send(event).unwrap();
        }
        let first = block_on(rx.first()).unwrap();
        assert_eq!(rx.drain(first), vec![0, 1, 2, 3]);
        // Refill while a producer keeps going: a batch never exceeds the capacity.
        for event in 4..8 {
            tx.try_send(event).unwrap();
        }
        let first = block_on(rx.first()).unwrap();
        let batch = rx.drain(first);
        assert!(batch.len() <= 4);
        assert_eq!(batch, vec![4, 5, 6, 7]);
    }

    #[test]
    fn full_queue_pushes_back_on_producers() {
        let (tx, mut rx) = channel::<u32>(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));

        // An async sender waits instead of failing, and resumes once the UI drains.
        let sender = tx.clone();
        let blocked = std::thread::spawn(move || sender.blocking_send(3));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !blocked.is_finished(),
            "producer should be waiting for room"
        );
        let first = block_on(rx.first()).unwrap();
        assert_eq!(rx.drain(first), vec![1, 2]);
        blocked.join().unwrap().unwrap();
        let first = block_on(rx.first()).unwrap();
        assert_eq!(rx.drain(first), vec![3]);
    }

    #[test]
    fn senders_see_closed_when_the_receiver_is_dropped() {
        let (tx, rx) = channel::<u32>(2);
        assert!(!tx.is_closed());
        drop(rx);
        assert!(tx.is_closed());
        assert!(matches!(tx.try_send(1), Err(TrySendError::Closed(1))));
        assert!(block_on(tx.send(1)).is_err());
    }

    #[test]
    fn receiver_ends_after_the_last_sender_and_still_yields_queued_events() {
        let (tx, mut rx) = channel::<u32>(4);
        let tx2 = tx.clone();
        tx.try_send(1).unwrap();
        drop(tx);
        tx2.try_send(2).unwrap();
        drop(tx2);
        let first = block_on(rx.first()).unwrap();
        assert_eq!(rx.drain(first), vec![1, 2]);
        assert!(block_on(rx.first()).is_none());
    }

    // ---- attached to an entity in a headless window ----------------------------------------

    struct Root;
    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[derive(Default)]
    struct Sink {
        batches: Vec<Vec<u32>>,
    }

    struct Harness {
        window: WindowHandle<Root>,
        sink: gpui_kit::Entity<Sink>,
        notifications: Rc<Cell<usize>>,
        task: Task<()>,
        tx: BridgeSender<u32>,
    }

    impl Harness {
        fn new(cx: &mut TestAppContext, capacity: usize) -> Self {
            let window = cx.add_window(|_, _| Root);
            let sink = cx.new(|_| Sink::default());
            let notifications = Rc::new(Cell::new(0));
            let (tx, rx) = channel(capacity);
            let counter = notifications.clone();
            let observer =
                cx.update(|cx| cx.observe(&sink, move |_, _| counter.set(counter.get() + 1)));
            observer.detach();
            let task = cx.update(|cx| {
                attach(&sink, window.into(), rx, cx, |sink: &mut Sink, batch, _| {
                    sink.batches.push(batch)
                })
            });
            Self {
                window,
                sink,
                notifications,
                task,
                tx,
            }
        }

        /// Lets the bridge task react to events, then delivers one frame.
        fn frame(&self, cx: &mut TestAppContext) {
            cx.run_until_parked();
            self.window
                .update(cx, |_, window, cx| {
                    window.simulate_next_frame(cx);
                })
                .unwrap();
            cx.run_until_parked();
        }

        fn batches(&self, cx: &mut TestAppContext) -> Vec<Vec<u32>> {
            self.sink.read_with(cx, |sink, _| sink.batches.clone())
        }
    }

    #[gpui_kit::test]
    fn a_burst_is_one_batch_and_one_notify_per_frame(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, 2048);
        for event in 0..1000 {
            harness.tx.try_send(event).unwrap();
        }
        cx.run_until_parked();
        // No frame yet: nothing has been applied.
        assert!(harness.batches(cx).is_empty());
        assert_eq!(harness.notifications.get(), 0);

        harness.frame(cx);
        let batches = harness.batches(cx);
        assert_eq!(batches.len(), 1, "one batch for the whole burst");
        assert_eq!(batches[0], (0..1000).collect::<Vec<_>>(), "order preserved");
        assert_eq!(harness.notifications.get(), 1, "one notify per batch");

        // An idle frame applies nothing and notifies nobody.
        harness.frame(cx);
        assert_eq!(harness.batches(cx).len(), 1);
        assert_eq!(harness.notifications.get(), 1);

        // The next event flows through the next frame as its own batch.
        harness.tx.try_send(1000).unwrap();
        harness.frame(cx);
        assert_eq!(harness.batches(cx).last().unwrap(), &vec![1000]);
        assert_eq!(harness.notifications.get(), 2);
    }

    #[gpui_kit::test]
    fn events_arriving_between_frames_wait_for_the_next_frame(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, 64);
        harness.tx.try_send(1).unwrap();
        cx.run_until_parked();
        harness.tx.try_send(2).unwrap(); // arrives after the wake-up, before the frame
        harness.frame(cx);
        assert_eq!(harness.batches(cx), vec![vec![1, 2]]);
        assert_eq!(harness.notifications.get(), 1);
    }

    #[gpui_kit::test]
    fn a_full_queue_is_applied_in_capacity_sized_batches(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, 4);
        for event in 0..4 {
            harness.tx.try_send(event).unwrap();
        }
        // Backpressure while the UI has not drained.
        assert!(matches!(harness.tx.try_send(4), Err(TrySendError::Full(4))));
        harness.frame(cx);
        harness.tx.try_send(4).unwrap();
        harness.frame(cx);
        assert_eq!(harness.batches(cx), vec![vec![0, 1, 2, 3], vec![4]]);
        assert_eq!(harness.notifications.get(), 2);
    }

    #[gpui_kit::test]
    fn dropping_the_entity_closes_the_bridge(cx: &mut TestAppContext) {
        let Harness {
            window,
            sink,
            task,
            tx,
            ..
        } = Harness::new(cx, 8);
        tx.try_send(1).unwrap();
        cx.run_until_parked();
        drop(sink); // the harness held the only strong handle
        window
            .update(cx, |_, window, cx| {
                window.simulate_next_frame(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            tx.is_closed(),
            "receiver is dropped once the entity is gone"
        );
        drop(task);
    }

    #[gpui_kit::test]
    fn dropping_the_task_stops_delivery(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, 8);
        drop(harness.task);
        harness.tx.try_send(1).unwrap();
        cx.run_until_parked();
        harness
            .window
            .update(cx, |_, window, cx| {
                window.simulate_next_frame(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            harness
                .sink
                .read_with(cx, |sink, _| sink.batches.is_empty())
        );
        assert_eq!(harness.notifications.get(), 0);
        assert!(harness.tx.is_closed());
    }

    #[gpui_kit::test]
    fn closing_all_senders_ends_the_task(cx: &mut TestAppContext) {
        let applied = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window(|_, _| Root);
        let sink = cx.new(|_| Sink::default());
        let (tx, rx) = channel::<u32>(8);
        let log = applied.clone();
        let task = cx.update(|cx| {
            attach(
                &sink,
                window.into(),
                rx,
                cx,
                move |_: &mut Sink, batch, _| log.borrow_mut().push(batch),
            )
        });
        tx.try_send(5).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                window.simulate_next_frame(cx);
            })
            .unwrap();
        drop(tx);
        cx.run_until_parked();
        assert_eq!(*applied.borrow(), vec![vec![5]]);
        drop(task);
    }
}
