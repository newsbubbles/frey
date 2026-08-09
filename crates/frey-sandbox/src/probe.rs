//! What this host can actually enforce, discovered at runtime.
//!
//! The rule that makes this module worth having: **never assume a control is available.** Landlock
//! in particular has to be probed rather than inferred from the platform, because a long-lived
//! enterprise distribution can ship a kernel without it, and even a kernel that has it will not
//! apply it unless the LSM was enabled at boot. Assuming it produces a process that believes it is
//! confined and is not — which is worse than knowing it is unconfined.

use frey_core::sandbox::{Availability, BackendId, Control, EnforcedSet};

/// A Linux Landlock ABI level, and what it can scope.
///
/// Levels matter because the controls arrived over several releases: filesystem scoping first, then
/// network ports, then abstract-socket and signal scoping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LandlockAbi(pub u8);

impl LandlockAbi {
    /// Landlock is not available at all.
    pub const NONE: Self = Self(0);
    /// The level at which TCP port scoping became available.
    pub const NETWORK: Self = Self(4);
    /// The level this crate treats as complete: filesystem, network, and IPC scoping.
    pub const FULL: Self = Self(6);

    /// Which controls this level can enforce.
    #[must_use]
    pub fn controls(self) -> EnforcedSet {
        let mut controls = Vec::new();
        if self.0 >= 1 {
            controls.push(Control::FilesystemRead);
            controls.push(Control::FilesystemWrite);
        }
        if self.0 >= Self::NETWORK.0 {
            controls.push(Control::NetworkEgress);
        }
        EnforcedSet::new(controls)
    }

    /// Whether anything at all can be enforced.
    #[must_use]
    pub fn is_available(self) -> bool {
        self.0 > 0
    }
}

/// What every backend enforces regardless of platform.
///
/// The resource limits come from ordinary process controls available everywhere. `ProgramAllowlist`
/// is here for a different reason worth stating: it is enforced by Frey **refusing to spawn**, in
/// `policy::validate`, rather than by the kernel. That makes it available on every host, including
/// one where no kernel confinement exists at all — and it is the control that matters most, since a
/// program that never starts cannot escape anything.
#[must_use]
pub fn baseline_controls() -> Vec<Control> {
    vec![
        Control::ProgramAllowlist,
        Control::MemoryLimit,
        Control::WallClockLimit,
        Control::ProcessLimit,
    ]
}

/// Describe a Linux host's confinement, given a detected Landlock level.
///
/// Split from detection so the reporting logic is testable on every platform — the interesting
/// cases are the *degraded* ones, and those cannot be reproduced by running on a good kernel.
#[must_use]
pub fn linux_availability(abi: LandlockAbi, lsm_enabled: bool) -> Availability {
    if !lsm_enabled {
        return Availability {
            usable: true,
            controls: EnforcedSet::new(baseline_controls()),
            detail:
                "landlock is compiled in but not enabled: add `landlock` to the kernel's `lsm=` \
                     boot parameter to get filesystem and network scoping"
                    .into(),
        };
    }
    if !abi.is_available() {
        return Availability {
            usable: true,
            controls: EnforcedSet::new(baseline_controls()),
            detail: "landlock is unavailable on this kernel; only resource limits are enforced"
                .into(),
        };
    }

    let mut controls: Vec<Control> = abi.controls().controls().to_vec();
    controls.extend(baseline_controls());
    Availability {
        usable: true,
        controls: EnforcedSet::new(controls),
        detail: format!("landlock ABI {}", abi.0),
    }
}

/// Describe a macOS host. Seatbelt profiles are applied by the kernel and inherited by every child,
/// and cannot be relaxed once applied, which is the semantics wanted here.
#[must_use]
pub fn macos_availability() -> Availability {
    let mut controls =
        vec![Control::FilesystemRead, Control::FilesystemWrite, Control::NetworkEgress];
    controls.extend(baseline_controls());
    Availability {
        usable: true,
        controls: EnforcedSet::new(controls),
        detail: "seatbelt (sandbox_init); note that Apple have deprecated the CLI wrapper".into(),
    }
}

/// Describe a Windows host. Without elevation an AppContainer cannot be created, so the fallback is
/// a low-integrity restricted token — which still confines, but less.
#[must_use]
pub fn windows_availability(elevated: bool) -> Availability {
    let mut controls = vec![Control::FilesystemRead, Control::FilesystemWrite];
    if elevated {
        controls.push(Control::NetworkEgress);
    }
    controls.extend(baseline_controls());
    Availability {
        usable: true,
        controls: EnforcedSet::new(controls),
        detail: if elevated {
            "appcontainer with zero capabilities, plus a job object".into()
        } else {
            "low-integrity restricted token plus a job object; network scoping needs elevation"
                .into()
        },
    }
}

/// Which backend this platform would use.
#[must_use]
pub fn backend_for_platform(elevated: bool) -> BackendId {
    if cfg!(target_os = "linux") {
        BackendId::Landlock
    } else if cfg!(target_os = "macos") {
        BackendId::Seatbelt
    } else if elevated {
        BackendId::WindowsAppContainer
    } else {
        BackendId::WindowsRestrictedToken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kernel_without_landlock_still_reports_what_it_can_do() {
        // RHEL 9 ships 5.14. Claiming filesystem scoping there would produce a process that
        // believes it is confined and is not.
        let availability = linux_availability(LandlockAbi::NONE, true);
        assert!(availability.usable);
        assert!(!availability.controls.has(Control::FilesystemRead));
        assert!(availability.controls.has(Control::MemoryLimit));
        assert!(availability.detail.contains("unavailable"));
    }

    #[test]
    fn landlock_compiled_in_but_not_booted_says_exactly_how_to_fix_it() {
        // The subtlest case: the kernel has it, the crate is present, and nothing is enforced.
        let availability = linux_availability(LandlockAbi::FULL, false);
        assert!(!availability.controls.has(Control::FilesystemRead));
        assert!(availability.detail.contains("lsm="), "{}", availability.detail);
    }

    #[test]
    fn network_scoping_needs_a_later_abi_than_filesystem_scoping() {
        let early = linux_availability(LandlockAbi(1), true);
        assert!(early.controls.has(Control::FilesystemWrite));
        assert!(!early.controls.has(Control::NetworkEgress), "ABI 1 cannot scope ports");

        let full = linux_availability(LandlockAbi::FULL, true);
        assert!(full.controls.has(Control::NetworkEgress));
        assert!(full.detail.contains("ABI 6"));
    }

    #[test]
    fn windows_without_elevation_confines_less_and_says_so() {
        let unelevated = windows_availability(false);
        assert!(unelevated.controls.has(Control::FilesystemWrite));
        assert!(!unelevated.controls.has(Control::NetworkEgress));
        assert!(unelevated.detail.contains("needs elevation"));

        assert!(windows_availability(true).controls.has(Control::NetworkEgress));
    }

    #[test]
    fn macos_reports_the_deprecation_it_depends_on() {
        // Apple have deprecated the CLI wrapper. An operator planning a fleet should know that from
        // the tool rather than from an outage.
        assert!(macos_availability().detail.contains("deprecated"));
    }

    #[test]
    fn every_platform_enforces_the_resource_baseline() {
        for availability in [
            linux_availability(LandlockAbi::NONE, false),
            macos_availability(),
            windows_availability(false),
        ] {
            for control in baseline_controls() {
                assert!(availability.controls.has(control), "{control:?} missing");
            }
        }
    }
}
