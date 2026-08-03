//! The NBD handle drop decision for host-agent shutdown.

/// What an `NbdHandle` drop does with the kernel side of its device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NbdDropAction {
    /// Normal operation: a dropped handle disconnects its device through
    /// netlink. This is correct for deliberate sandbox teardown.
    Disconnect,
    /// Shutdown is underway: leave the kernel configuration alive for the
    /// successor generation. An unwind or a late task drop must not sever a
    /// surviving guest's device before the successor reconfigures it.
    LeaveKernelConfigured,
}

/// Decide whether an NBD handle drop disconnects the kernel device.
/// `shutdown_underway` is the terminal abandon flag that the driver raises
/// before it aborts background tasks. The flag never clears.
pub fn nbd_drop_action(shutdown_underway: bool) -> NbdDropAction {
    if shutdown_underway {
        NbdDropAction::LeaveKernelConfigured
    } else {
        NbdDropAction::Disconnect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nbd_drop_disconnects_only_outside_shutdown() {
        assert_eq!(nbd_drop_action(false), NbdDropAction::Disconnect);
        assert_eq!(nbd_drop_action(true), NbdDropAction::LeaveKernelConfigured);
    }
}
