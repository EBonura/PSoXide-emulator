//! GPU facts for exact idle skipping. Read-only.

use super::*;

impl Gpu {
    /// Whether [`Gpu::decay_busy`] is currently its plain form, three
    /// saturating subtractions, so that one call with `n` cycles equals `n`
    /// calls with one: nothing queued or deferred in the DMA FIFO model.
    #[inline]
    pub(crate) fn decay_is_plain(&self) -> bool {
        !(self.experimental_dma_fifo
            && (!self.dma_input_fifo.is_empty() || self.deferred_irq_command.is_some()))
    }
}
