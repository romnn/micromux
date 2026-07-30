use crossterm::event::{KeyEvent, KeyEventKind, MouseEvent};
use futures::StreamExt;
use ratatui::crossterm::event::Event as CrosstermEvent;
use tokio::sync::mpsc;

const INPUT_QUEUE_CAPACITY: usize = 1024;

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
            Self::Event(CrosstermEvent::Mouse(MouseEvent {
                column, row, kind, ..
            })) => {
                write!(f, "Mouse({kind:?}, col={column}, row={row})")
            }
            other @ Self::Event(_) => std::fmt::Debug::fmt(other, f),
        }
    }
}

/// Terminal event handler.
#[derive(Debug)]
pub struct InputHandler {
    /// Event sender channel.
    sender: Option<mpsc::Sender<Input>>,
    /// Event receiver channel.
    receiver: mpsc::Receiver<Input>,
}

impl InputHandler {
    /// Constructs a dormant [`InputHandler`].
    ///
    /// The terminal reader starts on the first [`Self::next`] call, after terminal initialization
    /// has succeeded. This keeps construction safe in diagnostics and tests without a TTY.
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        Self {
            sender: Some(sender),
            receiver,
        }
    }

    /// Receives an event from the sender.
    ///
    /// This function blocks until an event is received.
    ///
    pub async fn next(&mut self) -> Option<Input> {
        if let Some(sender) = self.sender.take() {
            let actor = EventTask::new(sender);
            tokio::spawn(async move { actor.run().await });
        }
        self.receiver.recv().await
    }

    /// Returns one already-buffered event without waiting.
    pub fn try_next(&mut self) -> Option<Input> {
        self.receiver.try_recv().ok()
    }
}

/// Task that forwards terminal events until the receiver is dropped.
struct EventTask {
    /// Event sender channel.
    sender: mpsc::Sender<Input>,
}

impl EventTask {
    /// Constructs a new instance of [`EventTask`].
    fn new(sender: mpsc::Sender<Input>) -> Self {
        Self { sender }
    }

    /// Runs the event task, forwarding crossterm events until the receiver is dropped.
    async fn run(self) {
        let mut reader = crossterm::event::EventStream::new();
        loop {
            tokio::select! {
              () = self.sender.closed() => break,
              event = reader.next() => match event {
                Some(Ok(event)) => {
                    if !self.send(Input::Event(event)).await {
                        break;
                    }
                }
                Some(Err(_)) => {}
                None => break,
              },
            }
        }
    }

    /// Sends an event to the receiver.
    async fn send(&self, event: Input) -> bool {
        self.sender.send(event).await.is_ok()
    }
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::time::Duration;

    #[tokio::test]
    async fn bounded_event_queue_applies_backpressure() -> eyre::Result<()> {
        let (sender, mut receiver) = mpsc::channel(1);
        let task = EventTask::new(sender);
        assert!(task.send(Input::Event(CrosstermEvent::FocusGained)).await);

        let mut blocked =
            tokio::spawn(async move { task.send(Input::Event(CrosstermEvent::FocusLost)).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );

        assert_eq!(
            receiver.recv().await,
            Some(Input::Event(CrosstermEvent::FocusGained))
        );
        assert!(blocked.await?);
        assert_eq!(
            receiver.recv().await,
            Some(Input::Event(CrosstermEvent::FocusLost))
        );
        Ok(())
    }
}
