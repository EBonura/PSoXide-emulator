//! Platforms without a supported executable-memory implementation.

pub struct CodeBuffer;

impl CodeBuffer {
    pub fn new(_size: usize) -> Option<Self> {
        None
    }

    pub fn install(&mut self, _words: &[u32]) -> Option<*const u8> {
        None
    }
}
