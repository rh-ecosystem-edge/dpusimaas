use std::fmt;

/// The simulator a lease needs a slot for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulatorKind {
    Dpu,
    Host,
}

impl SimulatorKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dpu => "simulated DPU",
            Self::Host => "simulated host",
        }
    }
}

impl fmt::Display for SimulatorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Admission slots, separate by hypervisor and simulator kind.
#[derive(Clone, Copy, Debug)]
pub struct SlotLimits {
    pub dpus: usize,
    pub hosts: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotCounts {
    pub dpus: usize,
    pub hosts: usize,
}

impl SlotLimits {
    /// The slot that a lease of this mode would find full, if any.
    pub fn full_for(&self, mode: crate::lease::Mode, counts: SlotCounts) -> Option<SimulatorKind> {
        let counts = counts.with(mode);
        if counts.dpus > self.dpus {
            Some(SimulatorKind::Dpu)
        } else if counts.hosts > self.hosts {
            Some(SimulatorKind::Host)
        } else {
            None
        }
    }
}

impl SlotCounts {
    pub fn from_leases<'a>(leases: impl Iterator<Item = &'a crate::lease::Lease>) -> Self {
        leases.fold(Self::default(), |counts, lease| counts.with(lease.mode))
    }

    fn with(self, mode: crate::lease::Mode) -> Self {
        match mode {
            crate::lease::Mode::ZeroTrust => Self {
                dpus: self.dpus.saturating_add(1),
                hosts: self.hosts,
            },
            crate::lease::Mode::HostTrusted => Self {
                dpus: self.dpus.saturating_add(1),
                hosts: self.hosts.saturating_add(1),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::Mode;

    #[test]
    fn zero_trust_uses_only_a_dpu_slot() {
        let counts = SlotCounts::default().with(Mode::ZeroTrust);
        assert_eq!(counts, SlotCounts { dpus: 1, hosts: 0 });
    }

    #[test]
    fn host_trusted_uses_both_simulator_slots() {
        let counts = SlotCounts::default().with(Mode::HostTrusted);
        assert_eq!(counts, SlotCounts { dpus: 1, hosts: 1 });
    }

    #[test]
    fn reports_whichever_slot_is_full() {
        let limits = SlotLimits { dpus: 2, hosts: 1 };
        let full_host = SlotCounts { dpus: 1, hosts: 1 };
        assert_eq!(
            limits.full_for(Mode::HostTrusted, full_host),
            Some(SimulatorKind::Host)
        );

        let full_dpu = SlotCounts { dpus: 2, hosts: 0 };
        assert_eq!(
            limits.full_for(Mode::ZeroTrust, full_dpu),
            Some(SimulatorKind::Dpu)
        );
    }
}
