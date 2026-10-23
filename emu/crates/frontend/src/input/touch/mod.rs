//! The on-screen controller: layout, pointer state machine and preferences.
//! Pure logic only; drawing is in `ui::touch_overlay` and the device that
//! feeds the hub is `backend::touch`.

pub(crate) mod layout;
pub(crate) mod pad;
pub(crate) mod prefs;
