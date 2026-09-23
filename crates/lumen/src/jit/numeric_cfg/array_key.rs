//! Reuse an exact array-index conversion across adjacent reads of unchanged numeric values.
use super::plan::Step;

pub(super) struct Cache {
    register: Option<u32>,
    enabled: bool,
}

impl Cache {
    pub(super) fn new() -> Self {
        Self {
            register: None,
            enabled: std::env::var_os("LUMEN_JIT_NO_CFG_KEY_REUSE").is_none(),
        }
    }

    pub(super) fn before(&mut self, step: Step) {
        // Loading another numeric home emits no integer instruction and cannot change the
        // converted key. Clearing for everything else avoids reasoning about overwritten homes
        // or the x9 scratch convention outside array reads.
        if !matches!(step, Step::Load(_)) {
            self.register = None;
        }
    }

    pub(super) fn reused(&self, register: u32) -> bool {
        self.enabled && self.register == Some(register)
    }

    pub(super) fn record(&mut self, key: u32, result: u32) {
        self.register = (self.enabled && key != result).then_some(key);
    }

    pub(super) fn clear(&mut self) {
        self.register = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_only_unchanged_non_result_registers() {
        let mut cache = Cache::new();
        cache.record(16, 24);
        cache.before(Step::Load(0));
        assert!(cache.reused(16));
        assert!(!cache.reused(17));

        cache.before(Step::Constant(0));
        assert!(!cache.reused(16));

        cache.record(16, 16);
        cache.before(Step::Load(0));
        assert!(!cache.reused(16));
    }
}
