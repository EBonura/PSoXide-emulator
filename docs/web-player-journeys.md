# Web player journeys

The browser player (itch.io HTML5 and GitHub Pages) is one state machine with a handful of
inputs. This page maps every journey a player walks, says what must happen, and records the
automated check that proves it. The rules came from three bug reports (a Reset that dropped
the player into the menu, a disc pick that "re-downloaded" and never ran, a menu that came
back by itself) and are enforced by `src/session.rs` plus the browser suite in
`tools/web-journeys`.

## Rules

- Reset restarts the loaded game in place. It never opens the menu and never downloads.
- Picking the disc that is already loaded restarts it from the image in hand (the in-memory
  chunks, with the browser cache behind them). Picking another disc loads it; its data comes
  from the cache when it was fetched before.
- Closing the menu (PS button, Select+Start, Back, the touch Menu button) resumes exactly where
  the game was, unless the player had paused it themselves, and then it stays paused.
- The menu never reopens by itself. The only exception is a guest fault, which shows the menu
  so the player can reset.
- Nothing needs a page refresh.
- Every device presents an analog DualShock from the first poll in the browser (the demo disc
  needs one): keyboard, touchscreen and gamepads. Native keeps its own defaults (keyboard
  digital, pads DualShock) because digital-only retail games stop on "No Controller" otherwise.

## Journeys

| Journey | Phone (touch) | Desktop (keyboard, gamepad) |
|---|---|---|
| First visit | Page loads, "tap to play" splash, one tap starts audio and the demo disc. Controller shows, guest is analog. | The demo disc boots by itself, guest is analog, arrow keys drive the launcher with no F9. |
| Click or tap to play | The splash tap. | Nothing to click. |
| Play | 10 pad buttons, D-pad, two floating sticks, Menu and Fullscreen buttons. | Keyboard (arrows are D-pad and left stick, F/G/H/X face, Q/E/1/3 shoulders, Enter Start, Backspace Select, I/J/K/L right stick) or a gamepad in the standard mapping. |
| Open the menu | Menu button. Game pauses. | Esc, Select+Start, or the toolbar. Game pauses. |
| Close the menu | Menu button, or Back. Game resumes. | Esc, Select+Start, Circle/G, or Resume. Game resumes. |
| Reset | Menu, Game, Reset. Game restarts in place, menu closes, no download. | Toolbar Reset, menu Game / Reset, or the pad. Same. |
| Pick the same disc again | Menu, Library, Homebrew, PSoXide Demo Disc. Restarts, no download. | Same. |
| Pick another program | Library entry loads it; the running game keeps playing until the new one boots. | Same. |
| Rotate | Controller re-lays out, game and menu state untouched. | Resize keeps state. |
| Background and return | Held buttons release, the game keeps its state. | Same. |
| Reload the page | Splash again, disc boots from the browser cache. | The disc boots again. |
| Revisit | Volume, filters and key bindings persist (`localStorage`). | Same. |
| Browser without WebGPU | One plain sentence on the loader instead of a panic. | Same. |

## What was broken

1. **Reset** reseated the CPU at the BIOS reset vector. The web machine boots through the HLE
   kernel with no BIOS, so the CPU faulted on the first instruction, the frame loop stopped the
   game and opened the menu over a black screen.
2. **Picking the demo disc again** called `web_stream::start`, which refused to start a second
   time once the first delivery had handed its disc over, so the toast said "Downloading" and
   nothing happened. Behind it the machine was the one Reset had wrecked, so closing the menu
   ran it for a frame and the fault reopened the menu.
3. **Closing the menu with Back** left the game paused behind a closed menu, so the next press
   opened the menu again.
4. **Analog**: the keyboard started as an original digital pad (ID 0x41) and pads as a DualShock
   waiting for the handshake, so the launcher (which requires analog) said "This disc needs an
   analog controller" until F9.

Smaller fixes found by the suite: the save-states panel only had a mouse close button (Esc and
Back now close it); setting Touch controls back to Auto left the toolbar hidden; every setting
change toasted "settings save failed" in the browser (settings now persist in `localStorage`);
the iOS "No sound?" notice covered Select, Start and the sticks (it sits at the top now).

## Tests

State machine (native, `cargo test -p psoxide-emulator`): `src/session.rs` (13 transcripts of
the PS button, Back, Reset, pick, crash and a whole journey), `input::routing` (browser
defaults), `ui::menu` (panels close on Esc and Back), settings round trip.

Browser suite (`node tools/web-journeys/run.mjs`, Node 22 and Chrome, no dependencies). It
serves a built player itself, drives headless Chrome over the DevTools protocol with real
mouse, key and touch events, a faked Gamepad API, and reads the player through
`psoxideUxState()` and `psoxideTouchState()`. Phone runs use a Chrome whose display ratio
matches the emulated one (winit sizes the canvas from the real device pixel box).

    # build the player, then a site = the dist + a disc delivery
    trunk build --release --public-url ./ --dist dist     # in emu/crates/frontend, see build notes
    node tools/web-journeys/run.mjs --site <site dir> --shots <dir> --report report.md
    node tools/web-journeys/run.mjs --site <site dir> --only phone-portrait
    node tools/web-journeys/run.mjs --site <site dir> --scenario bench --query "?disc=games/quake.bin" --throttle 4

Speed (Quake menu scene through `?disc=`, headless Chrome on an M-series Mac; CPU throttle stands in
for a phone): 59.3 guest fps at 1x (6.3 ms of emulation per guest frame), 38.7 fps at 4x throttle,
32.4 fps at 6x. The demo disc launcher: 59.3 fps at 1x (4.3 ms), 52.6 fps at 4x. The wasm already
builds with fat LTO, one codegen unit, opt-level 3, `wasm-opt -O4` and simd128; wasm-opt -O3, -O4
and -O4 --converge, and simd128 off, all measured within noise of each other, so nothing changed.

Results on the landed build (chrome 130+ headless, macOS):

| Journey | Step | Result |
|---|---|---|
| desktop-direct | first visit: the demo disc boots by itself and runs | pass |
| desktop-direct | guest sees an analog DualShock with no F9 step | pass |
| desktop-direct | keyboard arrow reaches guest port 1 (left stick and D-pad) | pass |
| desktop-direct | keyboard Cross (F) reaches the guest | pass |
| desktop-direct | Esc opens the menu and pauses; Esc closes it and resumes the same machine | pass |
| desktop-direct | the menu does not reopen by itself | pass |
| desktop-direct | Back (Circle, G) closes the menu and resumes the game | pass |
| desktop-direct | toolbar Reset restarts the game in place: no menu, no download | pass |
| desktop-direct | menu Reset restarts the game in place and closes the menu | pass |
| desktop-direct | picking the loaded disc again restarts it with no download | pass |
| desktop-direct | a toolbar pause stays paused when a menu opened over it is closed | pass |
| desktop-direct | hiding and showing the tab leaves the game running with the menu closed | pass |
| desktop-direct | resizing the window (rotate) keeps the game and the menu state | pass |
| desktop-direct | a pad connected later is analog and drives the guest | pass |
| desktop-direct | reload: the player comes back running with the menu closed | pass |
| desktop-direct | page logged no errors | pass |
| desktop-iframe | first visit: the demo disc boots by itself and runs | pass |
| desktop-iframe | guest sees an analog DualShock with no F9 step | pass |
| desktop-iframe | keyboard arrow reaches guest port 1 (left stick and D-pad) | pass |
| desktop-iframe | keyboard Cross (F) reaches the guest | pass |
| desktop-iframe | Esc opens the menu and pauses; Esc closes it and resumes the same machine | pass |
| desktop-iframe | the menu does not reopen by itself | pass |
| desktop-iframe | Back (Circle, G) closes the menu and resumes the game | pass |
| desktop-iframe | toolbar Reset restarts the game in place: no menu, no download | pass |
| desktop-iframe | menu Reset restarts the game in place and closes the menu | pass |
| desktop-iframe | picking the loaded disc again restarts it with no download | pass |
| desktop-iframe | a toolbar pause stays paused when a menu opened over it is closed | pass |
| desktop-iframe | hiding and showing the tab leaves the game running with the menu closed | pass |
| desktop-iframe | resizing the window (rotate) keeps the game and the menu state | pass |
| desktop-iframe | a pad connected later is analog and drives the guest | pass |
| desktop-iframe | reload: the player comes back running with the menu closed | pass |
| desktop-iframe | page logged no errors | pass |
| desktop-menus | the player is up | pass |
| desktop-menus | Game / Resume (Enter) resumes the game and closes the menu | pass |
| desktop-menus | Game / Save state stores a browser quick-save | pass |
| desktop-menus | Game / Load state opens the save-states panel | pass |
| desktop-menus | the save-states panel closes with Back and leaves the menu usable | pass |
| desktop-menus | Game / Record input starts a recording (the game reboots); the same row stops it | pass |
| desktop-menus | Game / Replay last recording and Load input replay do not break the player | pass |
| desktop-menus | Game / Texture filter cycles | pass |
| desktop-menus | Settings / Video scale changes its value | pass |
| desktop-menus | Settings / Deinterlace changes its value | pass |
| desktop-menus | Settings / Volume changes its value | pass |
| desktop-menus | Settings / Mute changes its value | pass |
| desktop-menus | Settings / Menu opacity changes its value | pass |
| desktop-menus | Settings / UI scale changes its value | pass |
| desktop-menus | Settings / When the computer is slow changes its value | pass |
| desktop-menus | Settings / Touch controls changes its value | pass |
| desktop-menus | Settings / Touch opacity changes its value | pass |
| desktop-menus | Settings / Touch size changes its value | pass |
| desktop-menus | Settings / Touch haptics changes its value | pass |
| desktop-menus | Settings / Left-handed layout changes its value | pass |
| desktop-menus | Settings / UI scale back to 100% (so the later steps aim right) | pass |
| desktop-menus | Settings / Touch controls cycles back to Auto (desktop shows no pad) | pass |
| desktop-menus | Settings / Controls opens the controls panel; Esc closes it | pass |
| desktop-menus | Settings / About opens the card; Enter closes it | pass |
| desktop-menus | Library rows: Homebrew expands, the folder rows are present, and closing resumes | pass |
| desktop-menus | toolbar: save icon opens the save-states panel | pass |
| desktop-menus | toolbar: controller icon opens the controls panel | pass |
| desktop-menus | toolbar: pause stops the guest and play resumes it | pass |
| desktop-menus | toolbar: the volume slider and the mute icon change Settings / Volume and Mute | pass |
| desktop-menus | toolbar: debug and collapse buttons keep the game running | pass |
| desktop-menus | Settings / Fullscreen does not break the player | pass |
| desktop-menus | page logged no errors | pass |
| desktop-nowebgpu | the page says it needs WebGPU and shows no fatal box | pass |
| desktop-gamepad | the player is up | pass |
| desktop-gamepad | a pad connects as an analog DualShock and its buttons reach the guest | pass |
| desktop-gamepad | the left stick reaches the guest as an axis | pass |
| desktop-gamepad | Select+Start opens the menu; again closes it and resumes | pass |
| desktop-gamepad | pad navigation: D-pad moves the selection, Circle closes the menu and resumes | pass |
| desktop-gamepad | pad navigation: Cross on Game / Reset restarts the game in place | pass |
| desktop-gamepad | page logged no errors | pass |
| desktop-gamepad-iframe | the player is up | pass |
| desktop-gamepad-iframe | a pad connects as an analog DualShock and its buttons reach the guest | pass |
| desktop-gamepad-iframe | the left stick reaches the guest as an axis | pass |
| desktop-gamepad-iframe | Select+Start opens the menu; again closes it and resumes | pass |
| desktop-gamepad-iframe | pad navigation: D-pad moves the selection, Circle closes the menu and resumes | pass |
| desktop-gamepad-iframe | pad navigation: Cross on Game / Reset restarts the game in place | pass |
| desktop-gamepad-iframe | page logged no errors | pass |
| phone-portrait | first visit: the tap-to-play splash shows, then the demo disc runs | pass |
| phone-portrait | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-portrait | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-portrait | the D-pad registers all four directions | pass |
| phone-portrait | the left stick drags to an analog axis and recentres on release | pass |
| phone-portrait | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-portrait | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-portrait | picking the loaded disc from the touch menu restarts it without a download | pass |
| phone-portrait | rotating keeps the game running and re-lays the controller out | pass |
| phone-portrait | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-portrait | the fullscreen button does not break the player | pass |
| phone-portrait | reload comes back to the tap-to-play splash, then plays | pass |
| phone-portrait | page logged no errors | pass |
| phone-iframe | first visit: the tap-to-play splash shows, then the demo disc runs | pass |
| phone-iframe | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-iframe | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-iframe | the D-pad registers all four directions | pass |
| phone-iframe | the left stick drags to an analog axis and recentres on release | pass |
| phone-iframe | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-iframe | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-iframe | picking the loaded disc from the touch menu restarts it without a download | pass |
| phone-iframe | rotating keeps the game running and re-lays the controller out | pass |
| phone-iframe | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-iframe | the fullscreen button does not break the player | pass |
| phone-iframe | reload comes back to the tap-to-play splash, then plays | pass |
| phone-iframe | page logged no errors | pass |
| phone-landscape | first visit: the tap-to-play splash shows, then the demo disc runs | pass |
| phone-landscape | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-landscape | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-landscape | the D-pad registers all four directions | pass |
| phone-landscape | the left stick drags to an analog axis and recentres on release | pass |
| phone-landscape | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-landscape | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-landscape | picking the loaded disc from the touch menu restarts it without a download | pass |
| phone-landscape | rotating keeps the game running and re-lays the controller out | pass |
| phone-landscape | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-landscape | the fullscreen button does not break the player | pass |
| phone-landscape | reload comes back to the tap-to-play splash, then plays | pass |
| phone-landscape | page logged no errors | pass |
| phone-small | first visit: the tap-to-play splash shows, then the demo disc runs | pass |
| phone-small | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-small | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-small | the D-pad registers all four directions | pass |
| phone-small | the left stick drags to an analog axis and recentres on release | pass |
| phone-small | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-small | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-small | picking the loaded disc from the touch menu restarts it without a download | pass |
| phone-small | rotating keeps the game running and re-lays the controller out | pass |
| phone-small | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-small | the fullscreen button does not break the player | pass |
| phone-small | reload comes back to the tap-to-play splash, then plays | pass |
| phone-small | page logged no errors | pass |

