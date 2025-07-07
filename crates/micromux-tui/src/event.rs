use color_eyre::eyre::OptionExt;
use crossterm::event::{KeyEvent, KeyEventKind, MouseEvent, MouseEventKind};
use futures::{FutureExt, StreamExt};
use ratatui::crossterm::event::Event as CrosstermEvent;
use std::time::Duration;
use tokio::sync::mpsc;

/// The frequency at which tick events are emitted.
const DRAW_TICK_FPS: f64 = 10.0; // 10 fps

/// Representation of all possible events.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Hash)]
pub enum Input {
    /// An event that is emitted on a regular schedule.
    ///
    /// Use this event to run any code which has to run outside of being a direct response to a user
    /// event. e.g. polling external systems, updating animations, or rendering the UI based on a
    /// fixed frame rate.
    Tick,
    /// Crossterm events.
    ///
    /// These events are emitted by the terminal.
    Event(CrosstermEvent),
}

impl std::fmt::Display for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tick => write!(f, "Tick"),
            Self::Event(CrosstermEvent::Key(KeyEvent { code, kind, .. })) => match kind {
                KeyEventKind::Press => write!(f, "KeyPress({code:?})"),
                KeyEventKind::Release => write!(f, "KeyRelease({code:?})"),
                KeyEventKind::Repeat => write!(f, "KeyRepeat({code:?})"),
            },
            Self::Event(CrosstermEvent::Resize(cols, rows)) => {
                write!(f, "Resize(cols={cols}, rows={rows})")
            }
            // TODO match kind
            Self::Event(CrosstermEvent::Mouse(MouseEvent {
                column, row, kind, ..
            })) => {
                write!(f, "Mouse(col={column}, row={row})")
            }
            other => std::fmt::Debug::fmt(other, f),
        }
    }
}

/// Terminal event handler.
#[derive(Debug)]
pub struct InputHandler {
    /// Event sender channel.
    sender: mpsc::UnboundedSender<Input>,
    /// Event receiver channel.
    receiver: mpsc::UnboundedReceiver<Input>,
}

impl InputHandler {
    /// Constructs a new instance of [`EventHandler`] and spawns a new task to handle events.
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let actor = EventTask::new(sender.clone());
        tokio::spawn(async { actor.run().await });
        Self { sender, receiver }
    }

    /// Receives an event from the sender.
    ///
    /// This function blocks until an event is received.
    ///
    /// # Errors
    ///
    /// This function returns an error if the sender channel is disconnected. This can happen if an
    /// error occurs in the event thread. In practice, this should not happen unless there is a
    /// problem with the underlying terminal.
    pub async fn next(&mut self) -> color_eyre::Result<Input> {
        self.receiver
            .recv()
            .await
            .ok_or_eyre("Failed to receive event")
    }

    // /// Queue an app event to be sent to the event receiver.
    // ///
    // /// This is useful for sending events to the event handler which will be processed by the next
    // /// iteration of the application's event loop.
    // pub fn send(&mut self, app_event: AppEvent) {
    //     // Ignore the result as the receiver cannot be dropped while this struct still has a
    //     // reference to it
    //     let _ = self.sender.send(Event::App(app_event));
    // }
}

/// A thread that handles reading crossterm events and emitting tick events on a regular schedule.
struct EventTask {
    /// Event sender channel.
    sender: mpsc::UnboundedSender<Input>,
}

impl EventTask {
    /// Constructs a new instance of [`EventThread`].
    fn new(sender: mpsc::UnboundedSender<Input>) -> Self {
        Self { sender }
    }

    /// Runs the event thread.
    ///
    /// This function emits tick events at a fixed rate and polls for crossterm events in between.
    async fn run(self) -> color_eyre::Result<()> {
        let tick_rate = Duration::from_secs_f64(1.0 / DRAW_TICK_FPS);
        let mut reader = crossterm::event::EventStream::new();
        let mut tick = tokio::time::interval(tick_rate);
        loop {
            let tick_delay_fut = tick.tick();
            let crossterm_event_fut = reader.next().fuse();
            tokio::select! {
              _ = self.sender.closed() => {
                break;
              }
              _ = tick_delay_fut => {
                self.send(Input::Tick);
              }
              Some(Ok(event)) = crossterm_event_fut => {
                self.send(Input::Event(event));
              }
            };
        }
        Ok(())
    }

    /// Sends an event to the receiver.
    fn send(&self, event: Input) {
        // Ignores the result because shutting down the app drops the receiver, which causes the send
        // operation to fail. This is expected behavior and should not panic.
        let _ = self.sender.send(event);
    }
}
