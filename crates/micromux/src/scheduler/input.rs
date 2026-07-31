use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::{Command, MAX_PTY_INPUT_BATCH_BYTES, MAX_PTY_PASTE_BYTES, ServiceID};

/// Maximum PTY input retained across every queue and active write in one session.
const PTY_INPUT_GLOBAL_MAX_BYTES: usize = 8 * MAX_PTY_PASTE_BYTES;
/// Maximum PTY input one service may retain within the session-wide budget.
const PTY_INPUT_PER_SERVICE_MAX_BYTES: usize = 2 * MAX_PTY_PASTE_BYTES;
/// Maximum prepared inputs waiting for the scheduler, independent of their byte reservations.
const PTY_INPUT_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Default)]
struct InputBudgetState {
    used: usize,
    per_service: HashMap<Arc<str>, usize>,
}

#[derive(Debug)]
pub(super) struct InputBudget {
    state: Mutex<InputBudgetState>,
    max_bytes: usize,
    per_service_max_bytes: usize,
}

impl InputBudget {
    fn shared() -> Arc<Self> {
        Self::shared_with_limits(PTY_INPUT_GLOBAL_MAX_BYTES, PTY_INPUT_PER_SERVICE_MAX_BYTES)
    }

    #[cfg(test)]
    pub(super) fn shared_with_limit(max_bytes: usize) -> Arc<Self> {
        Self::shared_with_limits(max_bytes, max_bytes)
    }

    fn shared_with_limits(max_bytes: usize, per_service_max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InputBudgetState::default()),
            max_bytes,
            per_service_max_bytes,
        })
    }

    fn try_reserve(
        self: &Arc<Self>,
        service_id: Arc<str>,
        bytes: usize,
    ) -> Result<InputPermit, InputBudgetExhausted> {
        let mut state = self.state.lock();
        let next_global = state
            .used
            .checked_add(bytes)
            .ok_or(InputBudgetExhausted::Global)?;
        if next_global > self.max_bytes {
            return Err(InputBudgetExhausted::Global);
        }
        let service_used = state
            .per_service
            .get(service_id.as_ref())
            .copied()
            .unwrap_or(0);
        let next_service = service_used
            .checked_add(bytes)
            .ok_or(InputBudgetExhausted::Service)?;
        if next_service > self.per_service_max_bytes {
            return Err(InputBudgetExhausted::Service);
        }

        state.used = next_global;
        if bytes > 0 {
            state
                .per_service
                .insert(Arc::clone(&service_id), next_service);
        }
        drop(state);
        Ok(InputPermit {
            budget: Arc::clone(self),
            service_id,
            bytes,
        })
    }
}

#[derive(Clone, Copy)]
enum InputBudgetExhausted {
    Global,
    Service,
}

#[derive(Debug)]
pub(super) struct InputPermit {
    budget: Arc<InputBudget>,
    service_id: Arc<str>,
    bytes: usize,
}

impl InputPermit {
    fn absorb(&mut self, mut other: Self) {
        debug_assert!(Arc::ptr_eq(&self.budget, &other.budget));
        self.bytes += other.bytes;
        other.bytes = 0;
    }
}

impl Drop for InputPermit {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }

        let mut state = self.budget.state.lock();
        debug_assert!(state.used >= self.bytes);
        state.used = state.used.saturating_sub(self.bytes);
        debug_assert!(
            state.per_service.contains_key(self.service_id.as_ref()),
            "input reservation lost its per-service accounting"
        );
        let std::collections::hash_map::Entry::Occupied(mut service) =
            state.per_service.entry(Arc::clone(&self.service_id))
        else {
            return;
        };
        debug_assert!(*service.get() >= self.bytes);
        *service.get_mut() = service.get().saturating_sub(self.bytes);
        if *service.get() == 0 {
            service.remove();
        }
    }
}

/// Kind of terminal input retained by [`PreparedPtyInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyInputKind {
    /// Ordinary key input, bounded as one small write batch.
    Input,
    /// One bracketed paste, retained and queued atomically.
    Paste,
}

impl PtyInputKind {
    /// Lowercase label used in operator-facing diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Paste => "paste",
        }
    }

    const fn max_bytes(self) -> usize {
        match self {
            Self::Input => MAX_PTY_INPUT_BATCH_BYTES,
            Self::Paste => MAX_PTY_PASTE_BYTES,
        }
    }
}

/// PTY input that already owns space in the session-wide and per-service byte budgets.
///
/// The reservation follows the payload from the TUI backlog through the scheduler and into the
/// per-run writer queue. Dropping this value at any point releases the space.
#[derive(Debug)]
pub struct PreparedPtyInput {
    service_id: Arc<str>,
    data: Vec<u8>,
    kind: PtyInputKind,
    permit: InputPermit,
}

impl PreparedPtyInput {
    pub(super) fn prepare(
        budget: &Arc<InputBudget>,
        service_id: ServiceID,
        data: Vec<u8>,
        kind: PtyInputKind,
    ) -> Result<Self, PtyInputPrepareError> {
        let bytes = data.len();
        let max_bytes = kind.max_bytes();
        if bytes > max_bytes {
            return Err(PtyInputPrepareError::TooLarge {
                kind,
                bytes,
                max_bytes,
            });
        }
        let service_id = Arc::<str>::from(service_id);
        let permit = match budget.try_reserve(Arc::clone(&service_id), bytes) {
            Ok(permit) => permit,
            Err(InputBudgetExhausted::Global) => {
                return Err(PtyInputPrepareError::BudgetExhausted {
                    kind,
                    bytes,
                    max_bytes: budget.max_bytes,
                });
            }
            Err(InputBudgetExhausted::Service) => {
                return Err(PtyInputPrepareError::ServiceBudgetExhausted {
                    kind,
                    bytes,
                    max_bytes: budget.per_service_max_bytes,
                });
            }
        };
        Ok(Self {
            service_id,
            data,
            kind,
            permit,
        })
    }

    /// Number of payload bytes held by this reservation.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether this payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Input kind retained by this value.
    #[must_use]
    pub const fn kind(&self) -> PtyInputKind {
        self.kind
    }

    /// Target service id.
    #[must_use]
    pub fn service_id(&self) -> &str {
        &self.service_id
    }

    /// Raw payload bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Merge adjacent key input while preserving the shared byte reservation.
    ///
    /// # Errors
    ///
    /// Returns `other` unchanged when the payloads target different services or sessions, either
    /// value is a paste, or the combined batch would exceed [`MAX_PTY_INPUT_BATCH_BYTES`].
    pub fn try_merge(&mut self, other: Self) -> Result<(), Self> {
        if self.kind != PtyInputKind::Input
            || other.kind != PtyInputKind::Input
            || self.service_id != other.service_id
            || !Arc::ptr_eq(&self.permit.budget, &other.permit.budget)
            || self.data.len() + other.data.len() > MAX_PTY_INPUT_BATCH_BYTES
        {
            return Err(other);
        }
        self.data.extend(other.data);
        self.permit.absorb(other.permit);
        Ok(())
    }

    pub(super) fn into_parts(self) -> (Vec<u8>, PtyInputKind, InputPermit) {
        (self.data, self.kind, self.permit)
    }
}

/// Failure to reserve a PTY input payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PtyInputPrepareError {
    /// The payload exceeds its per-message bound.
    #[error("{kind:?} payload is {bytes} bytes, exceeding the {max_bytes}-byte limit")]
    TooLarge {
        /// Input kind being prepared.
        kind: PtyInputKind,
        /// Payload size.
        bytes: usize,
        /// Per-message limit.
        max_bytes: usize,
    },
    /// The session-wide retained-input budget has no room for the complete payload.
    #[error(
        "{kind:?} payload is {bytes} bytes, but the {max_bytes}-byte session input budget is full"
    )]
    BudgetExhausted {
        /// Input kind being prepared.
        kind: PtyInputKind,
        /// Payload size.
        bytes: usize,
        /// Session-wide limit.
        max_bytes: usize,
    },
    /// This service's retained-input share has no room for the complete payload.
    #[error(
        "{kind:?} payload is {bytes} bytes, but the {max_bytes}-byte per-service input budget is full"
    )]
    ServiceBudgetExhausted {
        /// Input kind being prepared.
        kind: PtyInputKind,
        /// Payload size.
        bytes: usize,
        /// Per-service limit.
        max_bytes: usize,
    },
}

/// Failure to enqueue prepared PTY input.
#[derive(Debug, thiserror::Error)]
pub enum PtyInputSendError {
    /// The scheduler input queue is currently full.
    #[error("scheduler input queue is full")]
    Full(PreparedPtyInput),
    /// The scheduler has stopped.
    #[error("scheduler input queue is closed")]
    Closed(PreparedPtyInput),
    /// The payload was prepared for another session.
    #[error("PTY input belongs to another session")]
    WrongSession(PreparedPtyInput),
}

/// Trusted terminal capability for bounded PTY input and resize requests.
///
/// Clones share global and per-service byte budgets. Preparing input reserves its bytes before it
/// can enter any application or scheduler queue, so both limits cover the complete path through
/// the active PTY write.
#[derive(Clone)]
pub struct TerminalControl {
    commands: mpsc::Sender<Command>,
    input: mpsc::Sender<PreparedPtyInput>,
    budget: Arc<InputBudget>,
}

impl TerminalControl {
    pub(crate) fn channel(
        commands: mpsc::Sender<Command>,
    ) -> (Self, mpsc::Receiver<PreparedPtyInput>) {
        let (input, receiver) = mpsc::channel(PTY_INPUT_CHANNEL_CAPACITY);
        (
            Self {
                commands,
                input,
                budget: InputBudget::shared(),
            },
            receiver,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_byte_limit(
        commands: mpsc::Sender<Command>,
        capacity: usize,
        max_bytes: usize,
    ) -> (Self, mpsc::Receiver<PreparedPtyInput>) {
        let (input, receiver) = mpsc::channel(capacity);
        (
            Self {
                commands,
                input,
                budget: InputBudget::shared_with_limit(max_bytes),
            },
            receiver,
        )
    }

    /// Reserve one key-input batch.
    ///
    /// # Errors
    ///
    /// Returns [`PtyInputPrepareError`] when the batch, session-wide budget, or service share would
    /// be exceeded.
    pub fn prepare_input(
        &self,
        service_id: ServiceID,
        data: Vec<u8>,
    ) -> Result<PreparedPtyInput, PtyInputPrepareError> {
        PreparedPtyInput::prepare(&self.budget, service_id, data, PtyInputKind::Input)
    }

    /// Reserve one bracketed paste.
    ///
    /// # Errors
    ///
    /// Returns [`PtyInputPrepareError`] when the paste, session-wide budget, or service share would
    /// be exceeded.
    pub fn prepare_paste(
        &self,
        service_id: ServiceID,
        data: Vec<u8>,
    ) -> Result<PreparedPtyInput, PtyInputPrepareError> {
        PreparedPtyInput::prepare(&self.budget, service_id, data, PtyInputKind::Paste)
    }

    /// Enqueue input that already owns its global and per-service byte reservations.
    ///
    /// # Errors
    ///
    /// Returns the prepared payload when the input queue is full or closed, preserving the
    /// reservation so a caller can retry without an atomicity gap.
    pub fn try_send(&self, input: PreparedPtyInput) -> Result<(), PtyInputSendError> {
        if !Arc::ptr_eq(&self.budget, &input.permit.budget) {
            return Err(PtyInputSendError::WrongSession(input));
        }
        match self.input.try_send(input) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(input)) => Err(PtyInputSendError::Full(input)),
            Err(mpsc::error::TrySendError::Closed(input)) => Err(PtyInputSendError::Closed(input)),
        }
    }

    /// Queue a terminal resize without waiting for scheduler capacity.
    ///
    /// Returns `false` when the command queue is full or the scheduler has stopped.
    #[must_use]
    pub fn try_resize(&self, cols: u16, rows: u16) -> bool {
        self.commands
            .try_send(Command::ResizeAll { cols, rows })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    use super::*;

    #[test]
    fn reservation_covers_pending_and_queued_input() -> eyre::Result<()> {
        let (commands, _commands) = mpsc::channel(2);
        let (control, mut input) = TerminalControl::with_byte_limit(commands, 2, 4);
        let first = control.prepare_input("svc".to_string(), b"ab".to_vec())?;
        control.try_send(first)?;
        let second = control.prepare_input("svc".to_string(), b"cd".to_vec())?;

        assert_matches!(
            control.prepare_input("svc".to_string(), vec![b'e']),
            Err(PtyInputPrepareError::BudgetExhausted { .. })
        );
        drop(input.try_recv()?);
        let replacement = control.prepare_input("svc".to_string(), b"ef".to_vec())?;
        drop((second, replacement));
        Ok(())
    }

    #[test]
    fn merging_input_keeps_one_complete_reservation() -> eyre::Result<()> {
        let (commands, _commands) = mpsc::channel(2);
        let (control, _input) = TerminalControl::with_byte_limit(commands, 2, 4);
        let mut first = control.prepare_input("svc".to_string(), b"ab".to_vec())?;
        let second = control.prepare_input("svc".to_string(), b"cd".to_vec())?;

        first
            .try_merge(second)
            .map_err(|_| eyre::eyre!("adjacent input did not merge"))?;
        assert_eq!(first.data(), b"abcd");
        assert_matches!(
            control.prepare_input("svc".to_string(), vec![b'e']),
            Err(PtyInputPrepareError::BudgetExhausted { .. })
        );
        drop(first);
        control.prepare_input("svc".to_string(), b"abcd".to_vec())?;
        Ok(())
    }

    #[test]
    fn prepared_input_cannot_cross_session_queues() -> eyre::Result<()> {
        let (first_commands, _first_commands) = mpsc::channel(1);
        let (first, mut first_input) = TerminalControl::with_byte_limit(first_commands, 1, 4);
        let (second_commands, _second_commands) = mpsc::channel(1);
        let (second, mut second_input) = TerminalControl::with_byte_limit(second_commands, 1, 4);
        let mut prepared = first.prepare_input("svc".to_string(), vec![b'a'])?;
        let other = second.prepare_input("svc".to_string(), vec![b'b'])?;
        let other = prepared
            .try_merge(other)
            .expect_err("cross-session reservations must not merge");
        second.try_send(other)?;

        let prepared = match second.try_send(prepared) {
            Err(PtyInputSendError::WrongSession(prepared)) => prepared,
            other => eyre::bail!("cross-session input was not rejected: {other:?}"),
        };
        first.try_send(prepared)?;
        assert_eq!(first_input.try_recv()?.data(), b"a");
        assert_eq!(second_input.try_recv()?.data(), b"b");
        Ok(())
    }

    #[test]
    fn one_service_cannot_exhaust_the_session_input_budget() -> eyre::Result<()> {
        let budget = InputBudget::shared_with_limits(6, 4);
        let first = PreparedPtyInput::prepare(
            &budget,
            "first".to_string(),
            b"abcd".to_vec(),
            PtyInputKind::Input,
        )?;

        assert_matches!(
            PreparedPtyInput::prepare(
                &budget,
                "first".to_string(),
                vec![b'e'],
                PtyInputKind::Input,
            ),
            Err(PtyInputPrepareError::ServiceBudgetExhausted { .. })
        );
        let second = PreparedPtyInput::prepare(
            &budget,
            "second".to_string(),
            b"ef".to_vec(),
            PtyInputKind::Input,
        )?;
        assert_matches!(
            PreparedPtyInput::prepare(
                &budget,
                "third".to_string(),
                vec![b'g'],
                PtyInputKind::Input,
            ),
            Err(PtyInputPrepareError::BudgetExhausted { .. })
        );

        drop((first, second));
        PreparedPtyInput::prepare(
            &budget,
            "first".to_string(),
            b"abcd".to_vec(),
            PtyInputKind::Input,
        )?;
        Ok(())
    }
}
