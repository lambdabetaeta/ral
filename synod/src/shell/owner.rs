//! Registration and the unique handoff to window shutdown.

pub(super) enum Owner<H> {
    Open(Option<H>),
    Closing,
}

impl<H> Default for Owner<H> {
    fn default() -> Self {
        Self::Open(None)
    }
}

impl<H> Owner<H> {
    pub(super) fn current(&self) -> Option<&H> {
        match self {
            Self::Open(handle) => handle.as_ref(),
            Self::Closing => None,
        }
    }

    pub(super) fn open(&mut self) -> Option<&mut Option<H>> {
        match self {
            Self::Open(handle) => Some(handle),
            Self::Closing => None,
        }
    }

    pub(super) fn begin_close(&mut self) -> Option<Close<H>> {
        match std::mem::replace(self, Self::Closing) {
            Self::Open(handle) => Some(Close(handle)),
            Self::Closing => None,
        }
    }
}

/// Not cloneable: only the first close request can finish shutdown.
#[must_use]
pub(super) struct Close<H>(Option<H>);

impl<H> Close<H> {
    pub(super) fn finish(self, shutdown: impl FnOnce(H)) {
        if let Some(handle) = self.0 {
            shutdown(handle);
        }
    }
}
