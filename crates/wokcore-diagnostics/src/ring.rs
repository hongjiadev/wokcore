use std::{collections::VecDeque, fmt};

use crate::event::{
    DiagnosticBuildError, EventId, MAX_PREPARED_EVENT_BYTES, PreparedDiagnosticEvent,
};

pub const DEFAULT_RING_BYTES: usize = 16_777_216;
pub const MAX_RING_BYTES: usize = 16_777_216;
pub const DEFAULT_PAGE_EVENTS: usize = 100;
pub const MAX_PAGE_EVENTS: usize = 1_000;
pub const MAX_PAGE_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RingInsertOutcome {
    Inserted,
    Oversized,
    OutOfOrder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCursor {
    sequence: u64,
    event_id: EventId,
}

impl PageCursor {
    pub(crate) const fn new(sequence: u64, event_id: EventId) -> Self {
        Self { sequence, event_id }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRequest {
    direction: PageDirection,
    cursor: Option<PageCursor>,
    max_events: usize,
    max_bytes: usize,
}

impl PageRequest {
    pub const fn default_for(direction: PageDirection) -> Self {
        Self {
            direction,
            cursor: None,
            max_events: DEFAULT_PAGE_EVENTS,
            max_bytes: MAX_PAGE_BYTES,
        }
    }

    pub fn with_limits(
        direction: PageDirection,
        cursor: Option<PageCursor>,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Self, DiagnosticBuildError> {
        if max_events == 0
            || max_events > MAX_PAGE_EVENTS
            || !(MAX_PREPARED_EVENT_BYTES..=MAX_PAGE_BYTES).contains(&max_bytes)
        {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        Ok(Self {
            direction,
            cursor,
            max_events,
            max_bytes,
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RingPage {
    events: Box<[PreparedDiagnosticEvent]>,
    encoded_bytes: usize,
    ring_retained_bytes: usize,
    next_cursor: Option<PageCursor>,
}

impl RingPage {
    pub fn events(&self) -> &[PreparedDiagnosticEvent] {
        &self.events
    }

    pub const fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }

    pub const fn ring_retained_bytes(&self) -> usize {
        self.ring_retained_bytes
    }

    pub const fn next_cursor(&self) -> Option<PageCursor> {
        self.next_cursor
    }
}

impl fmt::Debug for RingPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RingPage([redacted])")
    }
}

pub(crate) struct DiagnosticRing {
    byte_budget: usize,
    retained_bytes: usize,
    last_sequence: Option<u64>,
    events: VecDeque<PreparedDiagnosticEvent>,
}

impl DiagnosticRing {
    pub(crate) fn new() -> Self {
        Self {
            byte_budget: DEFAULT_RING_BYTES,
            retained_bytes: 0,
            last_sequence: None,
            events: VecDeque::new(),
        }
    }

    pub(crate) fn with_byte_budget(byte_budget: usize) -> Result<Self, DiagnosticBuildError> {
        if !(MAX_PREPARED_EVENT_BYTES..=MAX_RING_BYTES).contains(&byte_budget) {
            return Err(DiagnosticBuildError::InvalidValue);
        }
        Ok(Self {
            byte_budget,
            retained_bytes: 0,
            last_sequence: None,
            events: VecDeque::new(),
        })
    }

    pub(crate) fn insert(&mut self, event: PreparedDiagnosticEvent) -> RingInsertOutcome {
        if self
            .last_sequence
            .is_some_and(|sequence| event.sequence() <= sequence)
        {
            return RingInsertOutcome::OutOfOrder;
        }
        let event_bytes = event.encoded_len();
        if event_bytes > self.byte_budget {
            return RingInsertOutcome::Oversized;
        }
        while self
            .retained_bytes
            .checked_add(event_bytes)
            .is_none_or(|bytes| bytes > self.byte_budget)
        {
            let Some(evicted) = self.events.pop_front() else {
                return RingInsertOutcome::Oversized;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(evicted.encoded_len());
        }
        self.retained_bytes = self.retained_bytes.saturating_add(event_bytes);
        self.last_sequence = Some(event.sequence());
        self.events.push_back(event);
        RingInsertOutcome::Inserted
    }

    pub(crate) fn page(&self, request: PageRequest) -> RingPage {
        let mut selected = Vec::with_capacity(request.max_events);
        let mut encoded_bytes = 0usize;
        let cursor_key = request
            .cursor
            .map(|cursor| (cursor.sequence, cursor.event_id));
        let mut select = |event: &PreparedDiagnosticEvent| {
            let key = (event.sequence(), event.event_id());
            let after_cursor = match (request.direction, cursor_key) {
                (_, None) => true,
                (PageDirection::Ascending, Some(cursor)) => key > cursor,
                (PageDirection::Descending, Some(cursor)) => key < cursor,
            };
            if !after_cursor || selected.len() >= request.max_events {
                return selected.len() < request.max_events;
            }
            let Some(next_bytes) = encoded_bytes.checked_add(event.encoded_len()) else {
                return false;
            };
            if next_bytes > request.max_bytes {
                return false;
            }
            encoded_bytes = next_bytes;
            selected.push(event.clone());
            true
        };
        match request.direction {
            PageDirection::Ascending => {
                for event in &self.events {
                    if !select(event) {
                        break;
                    }
                }
            }
            PageDirection::Descending => {
                for event in self.events.iter().rev() {
                    if !select(event) {
                        break;
                    }
                }
            }
        }
        let next_cursor = selected
            .last()
            .map(|event| PageCursor::new(event.sequence(), event.event_id()));
        RingPage {
            events: selected.into_boxed_slice(),
            encoded_bytes,
            ring_retained_bytes: self.retained_bytes,
            next_cursor,
        }
    }
}

impl Default for DiagnosticRing {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DiagnosticRing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticRing([redacted])")
    }
}
