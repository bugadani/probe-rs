//! The target state that the compare backends share.

use std::time::Duration;

use super::Purpose;
use crate::flashing::{ActiveFlasher, FlashAlgorithm, FlashError, FlashProgress, Flasher, Verify};
use crate::probe::DebugProbeError;
use crate::{Core, Session};

/// Only the chain of a [`Flasher`] holds backends that use the flash algorithm.
const NO_FLASHER: &str = "a compare without a flasher has no algorithm backends";

/// Gives the compare backends access to the target.
///
/// The context calls `Init` when a backend uses the flash algorithm and it is not initialized.
/// It calls `UnInit` before it gives the session or the core to a backend, and at the end. A
/// backend that uses the algorithm and the core in turn causes one `Init` and `UnInit` each
/// time.
pub struct CompareContext<'a> {
    session: &'a mut Session,
    core_index: usize,
    flasher: Option<&'a mut Flasher>,
    progress: FlashProgress<'static>,
    initialized: bool,
    init_failed: bool,
    faulted: bool,
}

impl<'a> CompareContext<'a> {
    pub(crate) fn new(session: &'a mut Session, flasher: &'a mut Flasher) -> Self {
        Self {
            session,
            core_index: flasher.core_index,
            flasher: Some(flasher),
            progress: FlashProgress::empty(),
            initialized: false,
            init_failed: false,
            faulted: false,
        }
    }

    /// Creates a context for a chain that has no backends that use the flash algorithm.
    pub(crate) fn without_algorithm(session: &'a mut Session, core_index: usize) -> Self {
        Self {
            session,
            core_index,
            flasher: None,
            progress: FlashProgress::empty(),
            initialized: false,
            init_failed: false,
            faulted: false,
        }
    }

    /// Returns the session.
    ///
    /// If an earlier backend initialized the flash algorithm, this calls `UnInit` first, and
    /// returns its error.
    pub fn session(&mut self) -> Result<&mut Session, FlashError> {
        self.uninit()?;
        self.invalidate_code();

        Ok(self.session)
    }

    /// Returns the core that the flash algorithm runs on.
    ///
    /// If an earlier backend initialized the flash algorithm, this calls `UnInit` first, and
    /// returns its error.
    pub fn core(&mut self) -> Result<Core<'_>, FlashError> {
        let core_index = self.core_index;
        self.session()?.core(core_index).map_err(FlashError::Core)
    }

    /// Returns the flash algorithm of this compare, without loading it.
    pub(crate) fn flash_algorithm(&self) -> Option<&FlashAlgorithm> {
        self.flasher.as_deref().map(Flasher::flash_algorithm)
    }

    /// Returns the flash algorithm, loaded and initialized for [`Verify`].
    pub(crate) fn algorithm(&mut self) -> Result<ActiveFlasher<'_, 'static, Verify>, FlashError> {
        let flasher = self.flasher.as_deref_mut().expect(NO_FLASHER);
        let mut active = flasher.activate(self.session, &mut self.progress)?;
        if !self.initialized {
            if let Err(error) = active.init(None) {
                self.init_failed = true;
                return Err(error);
            }
            self.initialized = true;
        }

        Ok(active)
    }

    /// Returns `true` if the flash algorithm cannot run after a backend error.
    pub(crate) fn faulted(&self) -> bool {
        self.faulted
    }

    /// Handles a backend error, and returns it if the compare must stop.
    ///
    /// A probe error always stops the compare, because each sector would wait for the same
    /// timeout. The context stops using the flash algorithm if the error can leave it unusable.
    pub(crate) fn absorb(&mut self, error: FlashError, purpose: Purpose) -> Result<(), FlashError> {
        if !purpose.tolerates_errors() || is_probe_error(&error) {
            return Err(error);
        }
        tracing::debug!("Ignoring a compare error: {error}");

        // A function that returns an error code leaves the initialized algorithm usable.
        if !(self.initialized && is_error_code(&error)) {
            self.handle_fault();
        }

        Ok(())
    }

    /// Stops using the flash algorithm if a backend error happened in `Init` or while it was
    /// initialized.
    fn handle_fault(&mut self) {
        // A timeout in `Init` leaves the core running.
        if !self.initialized && !self.init_failed {
            return;
        }
        self.faulted = true;
        self.stop_algorithm();
    }

    /// Halts the core, calls `UnInit` if the algorithm is initialized, and makes the next use
    /// write the code again.
    fn stop_algorithm(&mut self) {
        // The core can be in the fault handler of the application, or still run the algorithm.
        let halted = self
            .session
            .core(self.core_index)
            .and_then(|mut core| core.halt(Duration::from_millis(500)));
        if let Err(error) = halted {
            tracing::debug!("Halt after a compare error failed: {error}");
        }

        if let Err(error) = self.uninit() {
            tracing::debug!("Uninit after a compare error failed: {error}");
        }
        self.invalidate_code();
    }

    /// Makes the next use of the flash algorithm write its code again.
    ///
    /// A fault or a backend with session access can overwrite the code in target RAM.
    fn invalidate_code(&mut self) {
        if let Some(flasher) = self.flasher.as_deref_mut() {
            flasher.code_valid = false;
        }
    }

    /// Calls `UnInit` if the flash algorithm is initialized.
    pub(crate) fn finish(mut self, purpose: Purpose) -> Result<(), FlashError> {
        if let Err(error) = self.uninit() {
            self.absorb(error, purpose)?;
            // The state of the algorithm is not known. The next step loads it again.
            self.stop_algorithm();
        }

        Ok(())
    }

    fn uninit(&mut self) -> Result<(), FlashError> {
        if !self.initialized {
            return Ok(());
        }
        self.initialized = false;

        let flasher = self.flasher.as_deref_mut().expect(NO_FLASHER);
        let mut active = flasher.activate::<Verify>(self.session, &mut self.progress)?;
        active.uninit()
    }
}

/// Returns `true` if `error` or one of its sources satisfies `predicate`.
fn has_cause(
    error: &FlashError,
    predicate: impl Fn(&(dyn std::error::Error + 'static)) -> bool,
) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = current {
        if predicate(error) {
            return true;
        }
        current = error.source();
    }

    false
}

/// Returns `true` if the cause of `error` is the probe.
fn is_probe_error(error: &FlashError) -> bool {
    has_cause(error, |cause| cause.is::<DebugProbeError>())
}

/// Returns `true` if the cause of `error` is a flash algorithm function that returned an error
/// code.
fn is_error_code(error: &FlashError) -> bool {
    has_cause(error, |cause| {
        matches!(
            cause.downcast_ref::<FlashError>(),
            Some(FlashError::RoutineCallFailed { .. })
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::architecture::arm::ArmError;
    use crate::architecture::riscv::communication_interface::RiscvError;
    use crate::architecture::xtensa::communication_interface::XtensaError;

    #[test]
    fn probe_errors_at_any_depth() {
        let probe_errors = [
            Error::Probe(DebugProbeError::TargetNotFound),
            Error::Arm(ArmError::Probe(DebugProbeError::TargetNotFound)),
            Error::Riscv(RiscvError::DebugProbe(DebugProbeError::TargetNotFound)),
            Error::Xtensa(XtensaError::DebugProbe(DebugProbeError::TargetNotFound)),
            Error::Arm(ArmError::NoTargetResponse {
                source: Box::new(ArmError::Probe(DebugProbeError::TargetNotFound)),
            }),
        ];
        for error in probe_errors {
            assert!(is_probe_error(&FlashError::Core(error)));
        }

        let wrapped = FlashError::Init(Box::new(FlashError::UnableToReadCoreStatus(Error::Probe(
            DebugProbeError::TargetNotFound,
        ))));
        assert!(is_probe_error(&wrapped));

        assert!(!is_probe_error(&FlashError::Core(Error::Timeout)));
    }

    #[test]
    fn error_codes_at_any_depth() {
        let code = FlashError::FlashReadFailed {
            source: Box::new(FlashError::RoutineCallFailed {
                name: "read_flash".into(),
                error_code: 1,
            }),
        };
        assert!(is_error_code(&code));

        assert!(!is_error_code(&FlashError::Core(Error::Timeout)));
    }
}
