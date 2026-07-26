use wokcore_storage::SessionSourceErrorCode;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl TokenTotals {
    pub fn clamp_cache(mut self) -> Self {
        self.cache_read = self.cache_read.min(self.input);
        self.cache_write = self.cache_write.min(self.input);
        self
    }

    pub fn apply_cumulative(&mut self, current: Self) -> Option<Self> {
        let current = current.clamp_cache();
        let delta = Self {
            input: current.input.saturating_sub(self.input),
            output: current.output.saturating_sub(self.output),
            cache_read: current.cache_read.saturating_sub(self.cache_read),
            cache_write: current.cache_write.saturating_sub(self.cache_write),
            reasoning: current.reasoning.saturating_sub(self.reasoning),
        };
        *self = current;
        (!delta.is_zero()).then_some(delta)
    }

    pub fn add_last(&mut self, last: Self) -> Option<Self> {
        let last = last.clamp_cache();
        self.input = self.input.saturating_add(last.input);
        self.output = self.output.saturating_add(last.output);
        self.cache_read = self.cache_read.saturating_add(last.cache_read);
        self.cache_write = self.cache_write.saturating_add(last.cache_write);
        self.reasoning = self.reasoning.saturating_add(last.reasoning);
        (!last.is_zero()).then_some(last)
    }

    pub fn is_zero(self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.reasoning == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayResolution {
    NotForked,
    Resolved { replayed_events: u64 },
    Deferred(SessionSourceErrorCode),
}
