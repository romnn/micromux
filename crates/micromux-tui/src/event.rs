use color_eyre::eyre::OptionExt;
use crossterm::event::{KeyEvent, KeyEventKind, MouseEvent};
use futures::StreamExt;
use ratatui::crossterm::event::Event as CrosstermEvent;
use tokio::sync::mpsc;

/// Representation of all possible input events.
///
/// The TUI redraws on each of these and on every model [`micromux::SessionChange`], so there is no
/// periodic tick — nothing in the view is time-animated, and live output arrives as model changes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Hash)]
pub enum Input {
    /// Crossterm events emitted by the terminal.
    Event(CrosstermEvent),
}

impl std::fmt::Display for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
                column,
                row,
                kind: _,
                ..
            })) => {
                write!(f, "Mouse(col={column}, row={row})")
            }
            other @ Self::Event(_) => std::fmt::Debug::fmt(other, f),
        }
    }
}

/// Terminal event handler.
#[derive(Debug)]
pub struct InputHandler {
    /// Event sender channel.
    _sender: mpsc::UnboundedSender<Input>,
    /// Event receiver channel.
    receiver: mpsc::UnboundedReceiver<Input>,
}

impl InputHandler {
    /// Constructs a new instance of [`EventHandler`] and spawns a new task to handle events.
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let actor = EventTask::new(sender.clone());
        tokio::spawn(async { actor.run().await });
        Self {
            _sender: sender,
            receiver,
        }
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

    /// Runs the event thread, forwarding crossterm events until the receiver is dropped.
    async fn run(self) -> color_eyre::Result<()> {
        let mut reader = crossterm::event::EventStream::new();
        loop {
            tokio::select! {
              () = self.sender.closed() => break,
              event = reader.next() => match event {
                Some(Ok(event)) => self.send(Input::Event(event)),
                Some(Err(_)) => {}
                None => break,
              },
            }
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

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}
