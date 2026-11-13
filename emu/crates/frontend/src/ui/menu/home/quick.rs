//! The quick controller popup: what the toolbar's controller button opens over the game.
//!
//! A small panel under the button with the two ports (which controller is on each, its type and
//! whether analog mode is on, with a switch) and a button for the full Controls page. It is opened
//! on purpose and pauses the game while it is up. It is drawn in the shell's colours and type
//! (see `tokens`), takes the keyboard, a pad and the pointer, and says so when the game has locked
//! analog mode and the switch cannot act.

use egui::{Align2, Context, FontId, Id, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use super::{chamfer, fade, focus_cage, pages, tokens};
use crate::input::model::BackendKind;
use crate::input::routing::PsxPort;
use crate::input::{InputDeviceInfo, PadProfile};
use crate::ui::menu::MenuInput;
use emulator_core::pad::PadMode;

/// Panel width in points.
const WIDTH: f32 = 376.0;
/// Height of one port's block: the name and the type, plus a line for the switch.
fn row_height(row: &Row) -> f32 {
    match row.switch {
        Switch::Empty => 44.0,
        Switch::NoSticks => 66.0,
        Switch::Ready { .. } | Switch::Locked { .. } => 100.0,
    }
}
/// Where the panel sits below the top edge when no toolbar is known.
const TOP: f32 = 44.0;
const GAP: f32 = 12.0;
/// The two-line note under the ports.
const NOTE_H: f32 = 48.0;
const PAD: f32 = 16.0;

/// What the analog switch of a port can do right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Switch {
    /// The player can flip it; `on` is the mode the controller is in.
    Ready { on: bool },
    /// The game locked the controller's mode: the switch cannot act.
    Locked { on: bool },
    /// The device has no analog sticks (the keyboard, an original digital pad).
    NoSticks,
    /// Nothing is plugged into the port.
    Empty,
}

/// One port as the popup shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub port: PsxPort,
    /// The device whose mode the switch changes.
    pub device: Option<String>,
    pub name: String,
    /// "Digital pad" or "DualShock".
    pub kind: &'static str,
    pub switch: Switch,
}

/// The two ports from the connected devices, the guest pads' live modes and the games' locks.
///
/// A controller beats the keyboard on a shared port; the touch screen never takes a row.
pub(crate) fn rows(
    devices: &[InputDeviceInfo],
    modes: [Option<PadMode>; 2],
    locks: [bool; 2],
) -> Vec<Row> {
    [PsxPort::One, PsxPort::Two]
        .into_iter()
        .enumerate()
        .map(|(i, port)| {
            let on_port = || {
                devices
                    .iter()
                    .filter(move |d| d.port == port && d.identity.backend != BackendKind::Touch)
            };
            let Some(info) = on_port()
                .find(|d| d.identity.backend != BackendKind::Keyboard)
                .or_else(|| on_port().next())
            else {
                return Row {
                    port,
                    device: None,
                    name: "Nothing connected".into(),
                    kind: "",
                    switch: Switch::Empty,
                };
            };
            let keyboard = info.identity.backend == BackendKind::Keyboard;
            let digital = keyboard || info.profile == PadProfile::Digital;
            let on = modes[i].map_or(info.profile.is_analog(), |m| m == PadMode::Analog);
            let switch = if digital {
                Switch::NoSticks
            } else if locks[i] {
                Switch::Locked { on }
            } else {
                Switch::Ready { on }
            };
            Row {
                port,
                device: Some(info.identity.id.clone()),
                name: info.identity.name.clone(),
                kind: if digital { "Digital pad" } else { "DualShock" },
                switch,
            }
        })
        .collect()
}

/// What the popup asks the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Act {
    /// Put the game back as it was.
    Close,
    /// Switch a device's analog mode: `true` is analog.
    SetAnalog(String, bool),
    /// Open the full Controls page.
    Full,
}

/// One key or pad press for the popup, queued by the input step and taken by the next draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nav {
    Up,
    Down,
    Left,
    Right,
    Confirm,
}

/// The popup's own state. The app keeps one while it is open.
#[derive(Clone, Debug)]
pub(crate) struct Quick {
    /// Whether the game was running when the popup opened: closing gives it back that way.
    pub was_running: bool,
    /// The focused stop: the switches that can act, then the Full settings button.
    focus: usize,
    queued: Vec<Nav>,
}

impl Quick {
    pub(crate) fn new(was_running: bool) -> Self {
        Self {
            was_running,
            focus: 0,
            queued: Vec::new(),
        }
    }

    /// Queue this frame's presses. Back and the PS button are the caller's: they close the popup.
    pub(crate) fn press(&mut self, input: &MenuInput) {
        for (on, nav) in [
            (input.up, Nav::Up),
            (input.down, Nav::Down),
            (input.left, Nav::Left),
            (input.right, Nav::Right),
            (input.confirm, Nav::Confirm),
        ] {
            if on {
                self.queued.push(nav);
            }
        }
    }

    /// Draw the popup and return what the player chose. `top` is the bottom edge of the toolbar,
    /// `ignore` the toolbar's rect: a press there is the toolbar's, not an outside click.
    pub(crate) fn draw(
        &mut self,
        ctx: &Context,
        rows: &[Row],
        top: f32,
        ignore: Rect,
    ) -> Option<Act> {
        let screen = ctx.screen_rect();
        let width = WIDTH.min((screen.width() - 2.0 * GAP).max(200.0));
        let stops: Vec<Stop> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.switch, Switch::Ready { .. }))
            .map(|(i, _)| Stop::Switch(i))
            .chain(std::iter::once(Stop::Full))
            .collect();
        self.focus = self.focus.min(stops.len() - 1);
        let mut act = None;
        for nav in std::mem::take(&mut self.queued) {
            let current = stops[self.focus];
            match nav {
                Nav::Up => self.focus = self.focus.saturating_sub(1),
                Nav::Down => self.focus = (self.focus + 1).min(stops.len() - 1),
                Nav::Confirm | Nav::Left | Nav::Right => match current {
                    Stop::Full if nav == Nav::Confirm => act = Some(Act::Full),
                    Stop::Switch(i) => {
                        if let (Some(id), Switch::Ready { on }) =
                            (rows[i].device.clone(), &rows[i].switch)
                        {
                            let want = match nav {
                                Nav::Left => false,
                                Nav::Right => true,
                                _ => !*on,
                            };
                            if want != *on {
                                act = Some(Act::SetAnalog(id, want));
                            }
                        }
                    }
                    Stop::Full => {}
                },
            }
        }

        let height = PAD + 30.0 + rows.iter().map(row_height).sum::<f32>() + NOTE_H + 44.0 + PAD;
        let top = top.max(TOP - 10.0) + 8.0;
        let panel = Rect::from_min_size(
            Pos2::new(screen.right() - GAP - width, top),
            Vec2::new(width, height),
        );
        let a = ctx.animate_bool_with_time(Id::new("quick-controls-in"), true, 0.14);
        let slide = (1.0 - a) * 8.0;
        let panel = panel.translate(Vec2::new(0.0, -slide));

        let response = egui::Area::new(Id::new("quick-controls"))
            .order(egui::Order::Foreground)
            .fixed_pos(panel.min)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_min_size(panel.size());
                self.paint(ui, panel, rows, &stops, a, &mut act);
            });
        // A press outside the panel closes it, except on the toolbar (its button toggles).
        let pressed_at =
            ctx.input(|i| i.pointer.press_origin().filter(|_| i.pointer.any_pressed()));
        if let Some(at) = pressed_at {
            if !response.response.rect.contains(at) && !ignore.contains(at) && act.is_none() {
                act = Some(Act::Close);
            }
        }
        act
    }

    fn paint(
        &mut self,
        ui: &mut egui::Ui,
        panel: Rect,
        rows: &[Row],
        stops: &[Stop],
        a: f32,
        act: &mut Option<Act>,
    ) {
        let p = ui.painter().clone();
        // The ground, a hairline and the shell's one chamfered corner.
        let outline = chamfer(panel, tokens::CHAMFER);
        p.add(Shape::convex_polygon(
            outline.clone(),
            fade(tokens::GROUND, a),
            Stroke::new(1.0, fade(tokens::hairline_strong(), a)),
        ));
        let left = panel.left() + PAD;
        let right = panel.right() - PAD;
        let mut y = panel.top() + PAD;

        p.text(
            Pos2::new(left, y + 12.0),
            Align2::LEFT_CENTER,
            "Controllers",
            FontId::proportional(18.0),
            fade(tokens::TEXT, a),
        );
        p.text(
            Pos2::new(right - tokens::CHAMFER * 0.6, y + 12.0),
            Align2::RIGHT_CENTER,
            "Esc to close",
            FontId::proportional(tokens::CAPTION),
            fade(tokens::DIM, a),
        );
        y += 30.0;

        for (i, row) in rows.iter().enumerate() {
            let block =
                Rect::from_min_max(Pos2::new(left, y), Pos2::new(right, y + row_height(row)));
            p.hline(
                block.x_range(),
                block.top(),
                Stroke::new(1.0, fade(tokens::hairline(), a)),
            );
            // Line 1: the port and who is on it.
            p.text(
                Pos2::new(left, block.top() + 22.0),
                Align2::LEFT_CENTER,
                row.port.label().to_uppercase(),
                FontId::proportional(tokens::LABEL),
                fade(tokens::DIM, a),
            );
            let name_col = if row.switch == Switch::Empty {
                tokens::MUTED
            } else {
                tokens::TEXT
            };
            p.text(
                Pos2::new(left + 62.0, block.top() + 22.0),
                Align2::LEFT_CENTER,
                fit(&row.name, 30),
                FontId::proportional(tokens::BODY_LARGE),
                fade(name_col, a),
            );
            // Line 2: the type and the mode.
            let mode_line = match &row.switch {
                Switch::Ready { on } | Switch::Locked { on } => {
                    format!("{}  ·  analog {}", row.kind, if *on { "on" } else { "off" })
                }
                Switch::NoSticks => format!("{}  ·  no analog sticks", row.kind),
                Switch::Empty => String::new(),
            };
            p.text(
                Pos2::new(left + 62.0, block.top() + 46.0),
                Align2::LEFT_CENTER,
                mode_line,
                FontId::proportional(tokens::CAPTION_LARGE),
                fade(tokens::MUTED, a),
            );
            // Line 3: the switch, or why it cannot act.
            if let Switch::Ready { on } | Switch::Locked { on } = &row.switch {
                let locked = matches!(row.switch, Switch::Locked { .. });
                let line = Rect::from_min_max(
                    Pos2::new(left, block.top() + 64.0),
                    Pos2::new(right, block.top() + 96.0),
                );
                p.text(
                    Pos2::new(left + 10.0, line.center().y),
                    Align2::LEFT_CENTER,
                    if locked {
                        "Locked by the game"
                    } else {
                        "Analog mode"
                    },
                    FontId::proportional(tokens::CAPTION_LARGE),
                    fade(
                        if locked {
                            tokens::MUTED
                        } else {
                            tokens::TEXT_2
                        },
                        a,
                    ),
                );
                pages::draw_toggle(
                    &p,
                    right - 10.0,
                    line.center().y,
                    *on,
                    1.0,
                    if locked { a * 0.4 } else { a },
                );
                if !locked {
                    let r = ui.interact(line, Id::new(("quick-switch", i)), Sense::click());
                    remember(ui.ctx(), &format!("switch-{i}"), line);
                    if r.clicked() {
                        if let Some(id) = row.device.clone() {
                            *act = Some(Act::SetAnalog(id, !*on));
                        }
                    }
                    if r.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if stops.get(self.focus) == Some(&Stop::Switch(i)) {
                        focus_cage(&p, line, a, 1.0);
                    }
                }
            }
            y += row_height(row);
        }

        // The one line that says how analog changes on this setup.
        let note_top = y + 6.0;
        p.hline(
            Rect::from_min_max(Pos2::new(left, y), Pos2::new(right, y)).x_range(),
            y,
            Stroke::new(1.0, fade(tokens::hairline(), a)),
        );
        p.text(
            Pos2::new(left, note_top + 8.0),
            Align2::LEFT_CENTER,
            "Analog is switched here or on the Controls page.",
            FontId::proportional(tokens::CAPTION),
            fade(tokens::MUTED, a),
        );
        p.text(
            Pos2::new(left, note_top + 24.0),
            Align2::LEFT_CENTER,
            "The keyboard is always a digital pad.",
            FontId::proportional(tokens::CAPTION),
            fade(tokens::MUTED, a),
        );
        y = note_top + NOTE_H - 6.0;

        // The way to the full page.
        let button = Rect::from_min_max(Pos2::new(left, y), Pos2::new(right, y + 44.0));
        p.rect_filled(button, 0.0, fade(tokens::ACCENT, a * 0.14));
        p.rect_stroke(
            button,
            0.0,
            Stroke::new(1.0, fade(tokens::ACCENT, a * 0.7)),
            egui::StrokeKind::Inside,
        );
        p.text(
            button.center(),
            Align2::CENTER_CENTER,
            "Full controller settings",
            FontId::proportional(tokens::BODY_LARGE),
            fade(tokens::TEXT, a),
        );
        let r = ui.interact(button, Id::new("quick-full"), Sense::click());
        remember(ui.ctx(), "full", button);
        if r.clicked() {
            *act = Some(Act::Full);
        }
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if stops.get(self.focus) == Some(&Stop::Full) {
            focus_cage(&p, button, a, 1.0);
        }
    }
}

/// Keep where a control was drawn, so a test can press the real one.
fn remember(ctx: &Context, name: &str, rect: Rect) {
    ctx.data_mut(|d| d.insert_temp(Id::new(("quick-rect", name.to_string())), rect));
}

/// Where a control was last drawn: `full`, `switch-0`, `switch-1`.
#[cfg(test)]
pub(crate) fn rect_of(ctx: &Context, name: &str) -> Option<Rect> {
    ctx.data(|d| d.get_temp(Id::new(("quick-rect", name.to_string()))))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Switch(usize),
    Full,
}

/// A name that fits one line: cut with an ellipsis at `max` characters.
fn fit(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        name.to_string()
    } else {
        let cut: String = name.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::model::{DeviceIdentity, PadSnapshot};
    use crate::input::PortChoice;

    fn device(
        id: &str,
        name: &str,
        backend: BackendKind,
        port: PsxPort,
        profile: PadProfile,
    ) -> InputDeviceInfo {
        InputDeviceInfo {
            identity: DeviceIdentity {
                id: id.into(),
                name: name.into(),
                backend,
                vid_pid: None,
                mapping: "standard".into(),
                mapped: true,
            },
            port,
            choice: PortChoice::Auto,
            profile,
            ignored: false,
            live: PadSnapshot::default(),
        }
    }

    #[test]
    fn a_controller_beats_the_keyboard_on_a_shared_port() {
        let devices = [
            device(
                "kbd",
                "Keyboard",
                BackendKind::Keyboard,
                PsxPort::One,
                PadProfile::DualShock,
            ),
            device(
                "pad",
                "Xbox Controller",
                BackendKind::GameController,
                PsxPort::One,
                PadProfile::Analog,
            ),
            device(
                "touch",
                "Touchscreen",
                BackendKind::Touch,
                PsxPort::Two,
                PadProfile::Analog,
            ),
        ];
        let r = rows(&devices, [Some(PadMode::Analog), None], [false, false]);
        assert_eq!(r[0].name, "Xbox Controller");
        assert_eq!(r[0].switch, Switch::Ready { on: true });
        assert_eq!(r[1].switch, Switch::Empty, "touch never takes a row");
    }

    #[test]
    fn the_keyboard_has_no_switch_and_a_lock_disables_one() {
        let devices = [
            device(
                "kbd",
                "Keyboard",
                BackendKind::Keyboard,
                PsxPort::One,
                PadProfile::DualShock,
            ),
            device(
                "pad",
                "Pad",
                BackendKind::Gilrs,
                PsxPort::Two,
                PadProfile::DualShock,
            ),
        ];
        let r = rows(
            &devices,
            [Some(PadMode::Digital), Some(PadMode::Analog)],
            [false, true],
        );
        assert_eq!(r[0].switch, Switch::NoSticks);
        assert_eq!(r[1].switch, Switch::Locked { on: true });
    }

    #[test]
    fn the_switch_shows_the_mode_the_guest_pad_is_in() {
        let devices = [device(
            "pad",
            "Pad",
            BackendKind::Gilrs,
            PsxPort::One,
            PadProfile::DualShock,
        )];
        let off = rows(&devices, [Some(PadMode::Digital), None], [false, false]);
        let on = rows(&devices, [Some(PadMode::Analog), None], [false, false]);
        assert_eq!(off[0].switch, Switch::Ready { on: false });
        assert_eq!(on[0].switch, Switch::Ready { on: true });
    }
}
