//! The keyboard's stand-in gamepad: which key (or mouse button) drives each
//! gamepad control, in each bind table.
//!
//! Input reads the keys through it, and anything that shows a control to
//! the player names the key from it, so both agree. It knows nothing of
//! readings or devices. The control list and the idea of one table for both
//! come from imjustprism's key cap prompts (Marvisak/VIctory#3433).

use std::sync::atomic::{AtomicBool, Ordering};

/// Windows virtual keys the tables use. Mouse buttons are virtual keys too
/// (1, 2, 4, 5, 6); with mouse look the stand-in pad reads them.
pub const VK_LBUTTON: u8 = 0x01;
pub const VK_RBUTTON: u8 = 0x02;
pub const VK_MBUTTON: u8 = 0x04;
pub const VK_XBUTTON1: u8 = 0x05;
pub const VK_XBUTTON2: u8 = 0x06;
pub const VK_BACK: u8 = 0x08;
pub const VK_TAB: u8 = 0x09;
pub const VK_RETURN: u8 = 0x0D;
pub const VK_CAPITAL: u8 = 0x14;
pub const VK_ESCAPE: u8 = 0x1B;
pub const VK_SPACE: u8 = 0x20;
pub const VK_LEFT: u8 = 0x25;
pub const VK_UP: u8 = 0x26;
pub const VK_RIGHT: u8 = 0x27;
pub const VK_DOWN: u8 = 0x28;
pub const VK_F7: u8 = 0x76;
pub const VK_LSHIFT: u8 = 0xA0;
pub const VK_LCONTROL: u8 = 0xA2;

/// Declares the gamepad's controls and the list of all of them from one
/// list, so that the list cannot miss a control.
macro_rules! pad_controls {
    ($($control:ident),* $(,)?) => {
        /// A control of a gamepad.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum PadControl {
            $($control),*
        }

        impl PadControl {
            /// Every control, in the order declared.
            pub const ALL: &[Self] = &[$(Self::$control),*];
        }
    };
}

pad_controls!(
    Menu,
    View,
    A,
    B,
    X,
    Y,
    Up,
    Down,
    Left,
    Right,
    LeftShoulder,
    RightShoulder,
    LeftStick,
    RightStick,
    LeftTrigger,
    RightTrigger,
    LeftStickUp,
    LeftStickDown,
    LeftStickLeft,
    LeftStickRight,
    RightStickUp,
    RightStickDown,
    RightStickLeft,
    RightStickRight,
);

/// What a key does on the stand-in pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    Control(PadControl),
    /// Switches the rockstar binds between on foot and driving.
    SwitchLayout,
}

use Bind::{Control as C, SwitchLayout};
use PadControl as P;

/// The stand-in pad as it always was (`VICTORY_BINDS` unset or `classic`);
/// README lists it.
const CLASSIC: &[(u8, Bind)] = &[
    (VK_ESCAPE, C(P::Menu)),
    (VK_TAB, C(P::View)),
    (VK_RETURN, C(P::A)),
    (VK_BACK, C(P::B)),
    (b'X', C(P::X)),
    (b'Y', C(P::Y)),
    (VK_UP, C(P::Up)),
    (VK_DOWN, C(P::Down)),
    (VK_LEFT, C(P::Left)),
    (VK_RIGHT, C(P::Right)),
    (b'Q', C(P::LeftShoulder)),
    (b'E', C(P::RightShoulder)),
    (b'F', C(P::LeftStick)),
    (b'H', C(P::RightStick)),
    (b'Z', C(P::LeftTrigger)),
    (b'C', C(P::RightTrigger)),
    (b'W', C(P::LeftStickUp)),
    (b'S', C(P::LeftStickDown)),
    (b'A', C(P::LeftStickLeft)),
    (b'D', C(P::LeftStickRight)),
    (b'I', C(P::RightStickUp)),
    (b'K', C(P::RightStickDown)),
    (b'J', C(P::RightStickLeft)),
    (b'L', C(P::RightStickRight)),
];

/// Binds both rockstar layouts share: menus, phone, map, camera. Keys follow
/// the PC defaults of GTA V (and RDR2 where the two agree); controls follow
/// GTA V's pad layout. Where a control has several keys, the first one
/// listed is the one a prompt shows: the menu keys come first.
const ROCKSTAR_COMMON: &[(u8, Bind)] = &[
    (VK_ESCAPE, C(P::Menu)),
    (b'P', C(P::Menu)),
    (b'M', C(P::Menu)),
    (VK_BACK, C(P::B)),
    (VK_UP, C(P::Up)),
    (VK_DOWN, C(P::Down)),
    (VK_LEFT, C(P::Left)),
    (VK_RIGHT, C(P::Right)),
    (b'Z', C(P::Down)),
    (b'V', C(P::View)),
    (b'C', C(P::RightStick)),
    (b'F', C(P::Y)),
    (b'F', SwitchLayout),
    (VK_F7, SwitchLayout),
    (b'A', C(P::LeftStickLeft)),
    (b'D', C(P::LeftStickRight)),
    (b'I', C(P::RightStickUp)),
    (b'K', C(P::RightStickDown)),
    (b'J', C(P::RightStickLeft)),
    (b'L', C(P::RightStickRight)),
];

/// On foot: GTA V PC keys on GTA V pad controls.
const ROCKSTAR_FOOT: &[(u8, Bind)] = &[
    (b'W', C(P::LeftStickUp)),
    (b'S', C(P::LeftStickDown)),
    // E presses the pad's A (confirm, interact) and is the key its prompts show;
    // Enter does the same in menus. D-pad right, GTA V's context action, stays on
    // the right arrow.
    (b'E', C(P::A)),
    (VK_RETURN, C(P::A)),
    (VK_LSHIFT, C(P::A)),               // sprint
    (VK_SPACE, C(P::X)),                // jump, climb
    (b'R', C(P::B)),                    // reload, melee
    (b'Q', C(P::RightShoulder)),        // cover
    (VK_TAB, C(P::LeftShoulder)),       // weapon wheel
    (VK_LCONTROL, C(P::LeftStick)),     // stealth, crouch
    (VK_LBUTTON, C(P::RightTrigger)),   // attack
    (VK_RBUTTON, C(P::LeftTrigger)),    // aim
    (VK_MBUTTON, C(P::RightStick)),
];

/// Driving: W and S are the pedals, Space the handbrake.
const ROCKSTAR_DRIVE: &[(u8, Bind)] = &[
    (VK_RETURN, C(P::A)),
    (b'W', C(P::RightTrigger)),         // accelerate
    (b'S', C(P::LeftTrigger)),          // brake, reverse
    (VK_SPACE, C(P::RightShoulder)),    // handbrake
    (b'E', C(P::LeftStick)),            // horn
    (b'H', C(P::Right)),                // headlights
    (b'Q', C(P::Left)),                 // radio
    (b'R', C(P::B)),                    // cinematic camera
    (VK_TAB, C(P::LeftShoulder)),       // vehicle weapon wheel
    (VK_LBUTTON, C(P::RightShoulder)),  // drive-by fire
    (VK_RBUTTON, C(P::LeftShoulder)),   // drive-by aim
];

/// Which keys the stand-in pad reads (`VICTORY_BINDS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Binds {
    /// The table above, as it always was.
    #[default]
    Classic,
    /// Rockstar's PC defaults; F (enter or leave a vehicle) and F7 switch
    /// between the on-foot and driving layouts.
    Rockstar,
}

impl Binds {
    /// `VICTORY_BINDS`: `classic` (or unset) or `rockstar`; `Err` names an
    /// unknown value, which means classic.
    pub fn from_env() -> Result<Binds, String> {
        match std::env::var("VICTORY_BINDS").as_deref().map(str::trim) {
            Ok("rockstar") => Ok(Binds::Rockstar),
            Ok("classic" | "") | Err(_) => Ok(Binds::Classic),
            Ok(other) => Err(other.to_owned()),
        }
    }

    /// The binds in force, `driving` choosing the rockstar layout. Shared
    /// binds come first.
    pub fn table(self, driving: bool) -> impl Iterator<Item = &'static (u8, Bind)> {
        let (shared, layout): (&'static [(u8, Bind)], &'static [(u8, Bind)]) = match self {
            Binds::Classic => (CLASSIC, &[]),
            Binds::Rockstar if driving => (ROCKSTAR_COMMON, ROCKSTAR_DRIVE),
            Binds::Rockstar => (ROCKSTAR_COMMON, ROCKSTAR_FOOT),
        };
        shared.iter().chain(layout)
    }

    /// The key a prompt shows for `control`: the first bound to it.
    #[must_use]
    pub fn key(self, driving: bool, control: PadControl) -> Option<u8> {
        self.table(driving)
            .find(|(_, bind)| *bind == Bind::Control(control))
            .map(|(vk, _)| *vk)
    }
}

/// The rockstar binds are in their driving layout; input sets it, prompts
/// read it when they are drawn.
static DRIVING: AtomicBool = AtomicBool::new(false);

pub fn set_driving(driving: bool) {
    DRIVING.store(driving, Ordering::Relaxed);
}

#[must_use]
pub fn driving() -> bool {
    DRIVING.load(Ordering::Relaxed)
}

/// The key that drives `control` on the classic stand-in pad.
#[must_use]
pub fn fallback_key(control: PadControl) -> u8 {
    Binds::Classic
        .key(false, control)
        .expect("the classic table binds every control")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_classic_control_has_a_key_of_its_own() {
        let mut keys: Vec<u8> = PadControl::ALL.iter().map(|&c| fallback_key(c)).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), PadControl::ALL.len());
    }

    #[test]
    fn each_key_does_one_thing_per_layout() {
        for (binds, driving) in [
            (Binds::Classic, false),
            (Binds::Rockstar, false),
            (Binds::Rockstar, true),
        ] {
            let mut seen = Vec::new();
            for (vk, _) in binds.table(driving).filter(|(_, b)| *b != SwitchLayout) {
                assert!(!seen.contains(vk), "{binds:?} driving={driving}: {vk:#x} twice");
                seen.push(*vk);
            }
        }
    }

    #[test]
    fn prompts_name_the_key_of_the_layout_in_force() {
        let classic = |c| Binds::Classic.key(false, c);
        assert_eq!(classic(P::A), Some(VK_RETURN));
        assert_eq!(classic(P::LeftShoulder), Some(b'Q'));
        assert_eq!(classic(P::RightTrigger), Some(b'C'));

        let foot = |c| Binds::Rockstar.key(false, c);
        // Menu keys first where a control has several.
        assert_eq!(foot(P::A), Some(b'E'));
        assert_eq!(foot(P::B), Some(VK_BACK));
        assert_eq!(foot(P::Menu), Some(VK_ESCAPE));
        assert_eq!(foot(P::Down), Some(VK_DOWN));
        assert_eq!(foot(P::X), Some(VK_SPACE));
        assert_eq!(foot(P::Y), Some(b'F'));
        assert_eq!(foot(P::Right), Some(VK_RIGHT));
        assert_eq!(foot(P::RightShoulder), Some(b'Q'));
        assert_eq!(foot(P::LeftShoulder), Some(VK_TAB));
        assert_eq!(foot(P::LeftStick), Some(VK_LCONTROL));
        assert_eq!(foot(P::RightStick), Some(b'C'));
        assert_eq!(foot(P::RightTrigger), Some(VK_LBUTTON));
        assert_eq!(foot(P::LeftTrigger), Some(VK_RBUTTON));
        assert_eq!(foot(P::View), Some(b'V'));
        assert_eq!(foot(P::LeftStickUp), Some(b'W'));

        let drive = |c| Binds::Rockstar.key(true, c);
        assert_eq!(drive(P::A), Some(VK_RETURN));
        assert_eq!(drive(P::RightTrigger), Some(b'W'));
        assert_eq!(drive(P::LeftTrigger), Some(b'S'));
        assert_eq!(drive(P::RightShoulder), Some(VK_SPACE));
        assert_eq!(drive(P::LeftStick), Some(b'E'));
        assert_eq!(drive(P::LeftShoulder), Some(VK_TAB));
        // Driving has no forward on the stick, and no jump.
        assert_eq!(drive(P::LeftStickUp), None);
        assert_eq!(drive(P::X), None);
    }

    #[test]
    fn the_layout_switch_is_shared() {
        assert!(!driving());
        set_driving(true);
        assert!(driving());
        set_driving(false);
    }
}
