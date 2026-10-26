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
- The page opens on the home screen while the demo disc streams and boots behind it. On a first
  visit only, a line under the disc counts three seconds once the disc is ready and then starts
  it; any input (a key, a pad button or stick, a click, a tap) cancels the countdown and that
  input does nothing else. Later visits stay on the home screen until the player presses Start
  disc. The embedded player (`?embed=1`) has no home screen.
- Nothing needs a page refresh.
- Every device presents an analog DualShock from the first poll in the browser (the demo disc
  needs one): keyboard, touchscreen and gamepads. Native keeps its own defaults (keyboard
  digital, pads DualShock) because digital-only retail games stop on "No Controller" otherwise.

## Journeys

| Journey | Phone (touch) | Desktop (keyboard, gamepad) |
|---|---|---|
| First visit | Home opens at once with the Demo Disc case and a countdown line; with no input the disc starts. The first tap also unlocks sound. Controller shows once the game runs, guest is analog. | Same: home, countdown, then the demo disc runs, guest is analog, arrow keys drive the launcher with no F9. |
| Start disc | Start disc button. | Enter, Cross, Start disc, or the countdown. |
| Play | 10 pad buttons, D-pad, two floating sticks, Menu and Fullscreen buttons. | Keyboard (arrows are D-pad and left stick, F/G/H/X face, Q/E/1/3 shoulders, Enter Start, Backspace Select, I/J/K/L right stick) or a gamepad in the standard mapping. |
| Open the menu | Menu button. Game pauses. | Esc, Select+Start, or the toolbar. Game pauses. |
| Close the menu | Menu button, or Back. Game resumes. | Esc, Select+Start, Circle/G, or Resume. Game resumes. |
| Reset | Menu, Game, Reset. Game restarts in place, menu closes, no download. | Toolbar Reset, menu Game / Reset, or the pad. Same. |
| Pick the same disc again | Menu, Library, Homebrew, PSoXide Demo Disc. Restarts, no download. | Same. |
| Open a games folder | Home, the folder case, Choose folder (or Library, Open a games folder). | Same. Box art for the folder's discs is fetched by file name and cached; offline or a miss leaves a generated case. |
| Pick another program | Library entry loads it; the running game keeps playing until the new one boots. | Same. |
| Rotate | Controller re-lays out, game and menu state untouched. | Resize keeps state. |
| Background and return | Held buttons release, the game keeps its state. | Same. |
| Reload the page | Home again, no countdown, the disc comes from the browser cache. | Same. |
| Revisit | Volume, filters and key bindings persist (`localStorage`). | Same. |
| Browser without WebGPU | One plain sentence on the loader instead of a panic. | Same. |
| Fullscreen button | Enters fullscreen where the browser allows it. An iPhone has no Fullscreen API: inside another site's frame the button opens the player's own page in its own tab, on a page of its own it says how to Add to Home Screen (the page ships a web manifest with `display: fullscreen`). | The toolbar and the menu's Fullscreen row. |
| Reopen a saved games folder | Not offered. | "Reopen your games folder" appears in the Library only after a folder was connected on an earlier visit. |

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

Menu audit (web and desktop, every page): the texture filter was in both Game and Settings (now Settings
only); "Reconnect saved games" showed to everyone and meant nothing to most (now "Reopen your games folder",
only after a folder was connected); "Load games folder" is "Open a games folder" with its hint; "1 entries" is
"1 entry"; "When the computer is slow: Keep game speed / Keep it smooth" is "If the device is too slow: Skip
frames / Slow the game"; "Replay last recording" is left out on the web (a recording downloads as a file, so
there is nothing to replay); the banner no longer calls the optional folder a requirement. The suite walks
every row of every column and fails on a label that appears twice.

Controller layout: SELECT and START labels were drawn at 38% of the pill height and are now 34%, so the
widest label fills at most 70% of its pill on every device in the layout table (a unit test measures the real
font); a second test keeps everything drawn clear of the safe-area edges, and the browser suite checks every
drawn rect of the live layout against a phone with a notch and a home indicator (portrait and landscape).
The status toast clears the controller's top buttons when a notch pushes them down.

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

Results on the landed build (Chrome headless, macOS; WebKit was not driven, see NOTES): 271 of 271 steps pass.

| Journey | Step | Result |
|---|---|---|
| desktop-direct | first visit: the home screen counts down, then the demo disc runs | pass |
| desktop-direct | guest sees an analog DualShock with no F9 step | pass |
| desktop-direct | keyboard arrow reaches guest port 1 (left stick and D-pad) | pass |
| desktop-direct | keyboard Cross (F) reaches the guest | pass |
| desktop-direct | Esc opens the menu and pauses; Esc closes it and resumes the same machine | pass |
| desktop-direct | the menu does not reopen by itself | pass |
| desktop-direct | Back (Circle, G) closes the menu and resumes the game | pass |
| desktop-direct | toolbar Reset restarts the game in place: no menu, no download | pass |
| desktop-direct | menu Reset restarts the game in place and closes the menu | pass |
| desktop-direct | picking the loaded disc again (Start over on its case) restarts it with no download | pass |
| desktop-direct | a toolbar pause stays paused when a menu opened over it is closed | pass |
| desktop-direct | hiding and showing the tab leaves the game running with the menu closed | pass |
| desktop-direct | resizing the window (rotate) keeps the game and the menu state | pass |
| desktop-direct | a pad connected later is analog and drives the guest | pass |
| desktop-direct | reload: a return visit stays on the home screen until Start disc | pass |
| desktop-direct | page logged no errors | pass |
| desktop-home | first visit: Home opens at once on the demo disc, with the countdown inline and nothing on top | pass |
| desktop-home | 'What is on it' opens on demand and closes again | pass |
| desktop-home | the countdown is cancelled by a key and the visitor stays on Home | pass |
| desktop-home | a return visit stays on Home with no countdown, and Start disc starts the disc | pass |
| desktop-home | the countdown is cancelled by a click and the visitor stays on Home | pass |
| desktop-home | the countdown is cancelled by a pad button and the visitor stays on Home | pass |
| desktop-home | the countdown is cancelled by a stick and the visitor stays on Home | pass |
| desktop-home | Escape, the pad chord and the touch menu button go back on Home and never start the disc | pass |
| desktop-home | Back (Circle) never starts the disc either, and Enter still does | pass |
| desktop-home | Controls, Settings and Debug: each opens inside the shell, fits the screen, and Back returns Home | pass |
| desktop-home | the keyboard moves focus: right changes the case, up reaches the nav, Enter opens it | pass |
| desktop-home | L1/R1 (Q/E, PageUp/PageDown) cycle the sections from anywhere and wrap | pass |
| desktop-home | every section is left the same visible ways: Back, Escape, the nav, the logo and a swipe or tap | pass |
| desktop-home | the welcome pages page forward and back, and Close returns Home | pass |
| desktop-home | Settings rows are real controls: a toggle flips and a slider drags | pass |
| desktop-shell | Start disc plays the launch sequence (case opens, disc out, case recedes, tray, spin) and boots | pass |
| desktop-shell | the shell over the running game: the same nav, a Game section and the game's own case | pass |
| desktop-shell | every section opens inside the shell and fits; each is left by pad or keyboard or touch | pass |
| desktop-shell | Debug on a phone is a full page of the shell: one column, nothing over the game, a way back | pass |
| desktop-shell | Escape and the touch menu button leave the shell from Home and resume the same machine | pass |
| desktop-shell | page logged no errors | pass |
| desktop-controls | Controls opens inside the shell; each group fits the screen with 44 px targets and nothing overlaps | pass |
| desktop-controls | a device's port and analog toggle change the routing, and change back | pass |
| desktop-controls | a key is rebound by selecting it and pressing the new one; Escape cancels; Reset restores | pass |
| desktop-controls | binding a key another action holds swaps the two and says so; nothing is left unbound | pass |
| desktop-controls | the keyboard moves between rows and along a row, Enter starts a binding, Escape cancels, then goes Home | pass |
| desktop-controls | the touch rows work: a toggle flips and a slider drags | pass |
| desktop-controls | page logged no errors | pass |
| desktop-controls-narrow | Controls opens inside the shell; each group fits the screen with 44 px targets and nothing overlaps | pass |
| desktop-controls-narrow | a device's port and analog toggle change the routing, and change back | pass |
| desktop-controls-narrow | a key is rebound by selecting it and pressing the new one; Escape cancels; Reset restores | pass |
| desktop-controls-narrow | binding a key another action holds swaps the two and says so; nothing is left unbound | pass |
| desktop-controls-narrow | the keyboard moves between rows and along a row, Enter starts a binding, Escape cancels, then goes Home | pass |
| desktop-controls-narrow | the touch rows work: a toggle flips and a slider drags | pass |
| desktop-controls-narrow | page logged no errors | pass |
| desktop-throttled | at 4 frames a second, clicking the nav switches sections | pass |
| desktop-throttled | at 4 frames a second, PageDown and Escape switch sections without dropping a press | pass |
| desktop-throttled | at 4 frames a second, Enter starts the focused disc and the launch takes about 2.3 s | pass |
| desktop-throttled | at 4 frames a second, a click or tap on Start disc launches, and any input skips the sequence | pass |
| desktop-throttled | at 4 frames a second, a quick pad press (Cross) on the focused Start disc launches it | pass |
| desktop-covers | cover fetch (hit): the shelf asks the collections by file name | pass |
| desktop-covers | only discs this browser really played are labelled as recent | pass |
| desktop-covers | a cached cover is not fetched again after a reload, and a saved folder can be reopened | pass |
| desktop-covers | cover fetch (miss): the shelf asks the collections by file name | pass |
| desktop-covers | cover fetch (offline): the shelf asks the collections by file name | pass |
| desktop-covers | box art switched off in Settings: opening a folder fetches nothing | pass |
| desktop-covers | with no play history, a folder's discs are not offered as recents | pass |
| desktop-grouping | a folder of rips lists one case per game: no tracks, discs together | pass |
| desktop-cover-states | a cover being fetched shows a loading state on its case, then (hit) settles | pass |
| desktop-cover-states | a cover being fetched shows a loading state on its case, then (miss) settles | pass |
| desktop-selection | moving the focus lifts the new case and settles the old one | pass |
| desktop-selection | Reduce motion in Settings is a switch and the shell reports it | pass |
| desktop-iframe | first visit: the home screen counts down, then the demo disc runs | pass |
| desktop-iframe | guest sees an analog DualShock with no F9 step | pass |
| desktop-iframe | keyboard arrow reaches guest port 1 (left stick and D-pad) | pass |
| desktop-iframe | keyboard Cross (F) reaches the guest | pass |
| desktop-iframe | Esc opens the menu and pauses; Esc closes it and resumes the same machine | pass |
| desktop-iframe | the menu does not reopen by itself | pass |
| desktop-iframe | Back (Circle, G) closes the menu and resumes the game | pass |
| desktop-iframe | toolbar Reset restarts the game in place: no menu, no download | pass |
| desktop-iframe | menu Reset restarts the game in place and closes the menu | pass |
| desktop-iframe | picking the loaded disc again (Start over on its case) restarts it with no download | pass |
| desktop-iframe | a toolbar pause stays paused when a menu opened over it is closed | pass |
| desktop-iframe | hiding and showing the tab leaves the game running with the menu closed | pass |
| desktop-iframe | resizing the window (rotate) keeps the game and the menu state | pass |
| desktop-iframe | a pad connected later is analog and drives the guest | pass |
| desktop-iframe | reload: a return visit stays on the home screen until Start disc | pass |
| desktop-iframe | page logged no errors | pass |
| desktop-menus | the player is up | pass |
| desktop-menus | Game / Resume (Enter) resumes the game and closes the menu | pass |
| desktop-menus | Game / Save state stores a browser quick-save | pass |
| desktop-menus | Game / Load state opens the save-states panel | pass |
| desktop-menus | the save-states panel closes with Back and leaves the menu usable | pass |
| desktop-menus | Game / Record input starts a recording (the game reboots); the same row stops it | pass |
| desktop-menus | Game / Load input replay does not break the player | pass |
| desktop-menus | no control appears twice, and the Library offers no Reconnect on a fresh visit | pass |
| desktop-menus | Settings / Video scale changes its value | pass |
| desktop-menus | Settings / Texture filter changes its value | pass |
| desktop-menus | Settings / Deinterlace changes its value | pass |
| desktop-menus | Settings / Volume changes its value | pass |
| desktop-menus | Settings / Mute changes its value | pass |
| desktop-menus | Settings / Menu opacity changes its value | pass |
| desktop-menus | Settings / UI scale changes its value | pass |
| desktop-menus | Settings / If the device is too slow changes its value | pass |
| desktop-menus | Settings / Touch controls changes its value | pass |
| desktop-menus | Settings / Touch opacity changes its value | pass |
| desktop-menus | Settings / Touch size changes its value | pass |
| desktop-menus | Settings / Touch haptics changes its value | pass |
| desktop-menus | Settings / Left-handed layout changes its value | pass |
| desktop-menus | Settings / Box art from the internet changes its value | pass |
| desktop-menus | Settings / UI scale back to 100% (so the later steps aim right) | pass |
| desktop-menus | Settings / Touch controls cycles back to Auto (desktop shows no pad) | pass |
| desktop-menus | no control appears twice (Settings has no Controls row, the tab is the way in) | pass |
| desktop-menus | Settings / About opens the welcome pages; Next pages through them and Back closes them | pass |
| desktop-menus | Home shows the game's case, the demo disc and the folder case; Escape on Home resumes | pass |
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
| phone-portrait | first visit: the home screen counts down on a phone, then the demo disc runs | pass |
| phone-portrait | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-portrait | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-portrait | the D-pad registers all four directions | pass |
| phone-portrait | the left stick drags to an analog axis and recentres on release | pass |
| phone-portrait | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-portrait | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-portrait | Start over on the game's case from the touch menu restarts it without a download | pass |
| phone-portrait | rotating keeps the game running and re-lays the controller out | pass |
| phone-portrait | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-portrait | the fullscreen button does not break the player | pass |
| phone-portrait | reload comes back to the home screen; tapping Start disc plays | pass |
| phone-portrait | page logged no errors | pass |
| phone-iframe | first visit: the home screen counts down on a phone, then the demo disc runs | pass |
| phone-iframe | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-iframe | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-iframe | the D-pad registers all four directions | pass |
| phone-iframe | the left stick drags to an analog axis and recentres on release | pass |
| phone-iframe | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-iframe | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-iframe | Start over on the game's case from the touch menu restarts it without a download | pass |
| phone-iframe | rotating keeps the game running and re-lays the controller out | pass |
| phone-iframe | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-iframe | the fullscreen button does not break the player | pass |
| phone-iframe | reload comes back to the home screen; tapping Start disc plays | pass |
| phone-iframe | page logged no errors | pass |
| phone-home | first visit: Home opens at once on the demo disc, with the countdown inline and nothing on top | pass |
| phone-home | the home screen shows the shell header only: no touch controller on it | pass |
| phone-home | 'What is on it' opens on demand and closes again | pass |
| phone-home | the countdown is cancelled by a key and the visitor stays on Home | pass |
| phone-home | a return visit stays on Home with no countdown, and Start disc starts the disc | pass |
| phone-home | the countdown is cancelled by a tap and the visitor stays on Home | pass |
| phone-home | a return visit stays on Home with no countdown, and Start disc starts the disc | pass |
| phone-home | the countdown is cancelled by a pad button and the visitor stays on Home | pass |
| phone-home | the countdown is cancelled by a stick and the visitor stays on Home | pass |
| phone-home | Escape, the pad chord and the touch menu button go back on Home and never start the disc | pass |
| phone-home | Back (Circle) never starts the disc either, and Enter still does | pass |
| phone-home | Controls, Settings and Debug: each opens inside the shell, fits the screen, and Back returns Home | pass |
| phone-home | the keyboard moves focus: right changes the case, up reaches the nav, Enter opens it | pass |
| phone-home | L1/R1 (Q/E, PageUp/PageDown) cycle the sections from anywhere and wrap | pass |
| phone-home | every section is left the same visible ways: Back, Escape, the nav, the logo and a swipe or tap | pass |
| phone-home | the welcome pages page forward and back, and Close returns Home | pass |
| phone-home | Settings rows are real controls: a toggle flips and a slider drags | pass |
| phone-home-small | first visit: Home opens at once on the demo disc, with the countdown inline and nothing on top | pass |
| phone-home-small | the home screen shows the shell header only: no touch controller on it | pass |
| phone-home-small | 'What is on it' opens on demand and closes again | pass |
| phone-home-small | the countdown is cancelled by a key and the visitor stays on Home | pass |
| phone-home-small | a return visit stays on Home with no countdown, and Start disc starts the disc | pass |
| phone-home-small | the countdown is cancelled by a tap and the visitor stays on Home | pass |
| phone-home-small | a return visit stays on Home with no countdown, and Start disc starts the disc | pass |
| phone-home-small | the countdown is cancelled by a pad button and the visitor stays on Home | pass |
| phone-home-small | the countdown is cancelled by a stick and the visitor stays on Home | pass |
| phone-home-small | Escape, the pad chord and the touch menu button go back on Home and never start the disc | pass |
| phone-home-small | Back (Circle) never starts the disc either, and Enter still does | pass |
| phone-home-small | Controls, Settings and Debug: each opens inside the shell, fits the screen, and Back returns Home | pass |
| phone-home-small | the keyboard moves focus: right changes the case, up reaches the nav, Enter opens it | pass |
| phone-home-small | L1/R1 (Q/E, PageUp/PageDown) cycle the sections from anywhere and wrap | pass |
| phone-home-small | every section is left the same visible ways: Back, Escape, the nav, the logo and a swipe or tap | pass |
| phone-home-small | the welcome pages page forward and back, and Close returns Home | pass |
| phone-home-small | Settings rows are real controls: a toggle flips and a slider drags | pass |
| phone-home-landscape | first visit: Home opens at once on the demo disc, with the countdown inline and nothing on top | pass |
| phone-home-landscape | the home screen shows the shell header only: no touch controller on it | pass |
| phone-home-landscape | 'What is on it' opens on demand and closes again | pass |
| phone-home-landscape | the countdown is cancelled by a key and the visitor stays on Home | pass |
| phone-home-landscape | a return visit stays on Home with no countdown, and Start disc starts the disc | pass |
| phone-home-landscape | the countdown is cancelled by a tap and the visitor stays on Home | pass |
| phone-home-landscape | a return visit stays on Home with no countdown, and Start disc starts the disc | pass |
| phone-home-landscape | the countdown is cancelled by a pad button and the visitor stays on Home | pass |
| phone-home-landscape | the countdown is cancelled by a stick and the visitor stays on Home | pass |
| phone-home-landscape | Escape, the pad chord and the touch menu button go back on Home and never start the disc | pass |
| phone-home-landscape | Back (Circle) never starts the disc either, and Enter still does | pass |
| phone-home-landscape | Controls, Settings and Debug: each opens inside the shell, fits the screen, and Back returns Home | pass |
| phone-home-landscape | the keyboard moves focus: right changes the case, up reaches the nav, Enter opens it | pass |
| phone-home-landscape | L1/R1 (Q/E, PageUp/PageDown) cycle the sections from anywhere and wrap | pass |
| phone-home-landscape | every section is left the same visible ways: Back, Escape, the nav, the logo and a swipe or tap | pass |
| phone-home-landscape | the welcome pages page forward and back, and Close returns Home | pass |
| phone-home-landscape | Settings rows are real controls: a toggle flips and a slider drags | pass |
| phone-grouping | a folder of rips lists one case per game: no tracks, discs together | pass |
| phone-cover-states | a cover being fetched shows a loading state on its case, then (hit) settles | pass |
| phone-cover-states | a cover being fetched shows a loading state on its case, then (miss) settles | pass |
| phone-selection | moving the focus lifts the new case and settles the old one | pass |
| phone-selection | Reduce motion in Settings is a switch and the shell reports it | pass |
| phone-shell | Start disc plays the launch sequence (case opens, disc out, case recedes, tray, spin) and boots | pass |
| phone-shell | the shell over the running game: the same nav, a Game section and the game's own case | pass |
| phone-shell | every section opens inside the shell and fits; each is left by pad or keyboard or touch | pass |
| phone-shell | Debug on a phone is a full page of the shell: one column, nothing over the game, a way back | pass |
| phone-shell | the on-screen menu button answers mouse clicks as it does taps: a click resumes, a click pauses into the shell | pass |
| phone-shell | Escape and the touch menu button leave the shell from Home and resume the same machine | pass |
| phone-shell | page logged no errors | pass |
| phone-shell-small | Start disc plays the launch sequence (case opens, disc out, case recedes, tray, spin) and boots | pass |
| phone-shell-small | the shell over the running game: the same nav, a Game section and the game's own case | pass |
| phone-shell-small | every section opens inside the shell and fits; each is left by pad or keyboard or touch | pass |
| phone-shell-small | Debug on a phone is a full page of the shell: one column, nothing over the game, a way back | pass |
| phone-shell-small | the on-screen menu button answers mouse clicks as it does taps: a click resumes, a click pauses into the shell | pass |
| phone-shell-small | Escape and the touch menu button leave the shell from Home and resume the same machine | pass |
| phone-shell-small | page logged no errors | pass |
| phone-controls | Controls opens inside the shell; each group fits the screen with 44 px targets and nothing overlaps | pass |
| phone-controls | a device's port and analog toggle change the routing, and change back | pass |
| phone-controls | a key is rebound by selecting it and pressing the new one; Escape cancels; Reset restores | pass |
| phone-controls | binding a key another action holds swaps the two and says so; nothing is left unbound | pass |
| phone-controls | the touch rows work: a toggle flips and a slider drags | pass |
| phone-controls | page logged no errors | pass |
| phone-controls-small | Controls opens inside the shell; each group fits the screen with 44 px targets and nothing overlaps | pass |
| phone-controls-small | a device's port and analog toggle change the routing, and change back | pass |
| phone-controls-small | a key is rebound by selecting it and pressing the new one; Escape cancels; Reset restores | pass |
| phone-controls-small | binding a key another action holds swaps the two and says so; nothing is left unbound | pass |
| phone-controls-small | the touch rows work: a toggle flips and a slider drags | pass |
| phone-controls-small | page logged no errors | pass |
| phone-controls-landscape | Controls opens inside the shell; each group fits the screen with 44 px targets and nothing overlaps | pass |
| phone-controls-landscape | a device's port and analog toggle change the routing, and change back | pass |
| phone-controls-landscape | a key is rebound by selecting it and pressing the new one; Escape cancels; Reset restores | pass |
| phone-controls-landscape | binding a key another action holds swaps the two and says so; nothing is left unbound | pass |
| phone-controls-landscape | the touch rows work: a toggle flips and a slider drags | pass |
| phone-controls-landscape | page logged no errors | pass |
| phone-throttled | at 4 frames a second, clicking the nav switches sections | pass |
| phone-throttled | at 4 frames a second, PageDown and Escape switch sections without dropping a press | pass |
| phone-throttled | at 4 frames a second, Enter starts the focused disc and the launch takes about 2.3 s | pass |
| phone-throttled | at 4 frames a second, a click or tap on Start disc launches, and any input skips the sequence | pass |
| phone-shell-landscape | Start disc plays the launch sequence (case opens, disc out, case recedes, tray, spin) and boots | pass |
| phone-shell-landscape | the shell over the running game: the same nav, a Game section and the game's own case | pass |
| phone-shell-landscape | every section opens inside the shell and fits; each is left by pad or keyboard or touch | pass |
| phone-shell-landscape | Debug on a phone is a full page of the shell: one column, nothing over the game, a way back | pass |
| phone-shell-landscape | the on-screen menu button answers mouse clicks as it does taps: a click resumes, a click pauses into the shell | pass |
| phone-shell-landscape | Escape and the touch menu button leave the shell from Home and resume the same machine | pass |
| phone-shell-landscape | page logged no errors | pass |
| phone-landscape | first visit: the home screen counts down on a phone, then the demo disc runs | pass |
| phone-landscape | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-landscape | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-landscape | the D-pad registers all four directions | pass |
| phone-landscape | the left stick drags to an analog axis and recentres on release | pass |
| phone-landscape | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-landscape | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-landscape | Start over on the game's case from the touch menu restarts it without a download | pass |
| phone-landscape | rotating keeps the game running and re-lays the controller out | pass |
| phone-landscape | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-landscape | the fullscreen button does not break the player | pass |
| phone-landscape | reload comes back to the home screen; tapping Start disc plays | pass |
| phone-landscape | page logged no errors | pass |
| phone-fullscreen | the fullscreen button is offered even without the Fullscreen API | pass |
| phone-fullscreen | tapping it on a page of its own explains Add to Home Screen | pass |
| phone-fullscreen | the game keeps running and the controller still works | pass |
| phone-fullscreen | page logged no errors | pass |
| phone-fullscreen-iframe | the fullscreen button is offered even without the Fullscreen API | pass |
| phone-fullscreen-iframe | tapping it inside a frame opens the player's own page in a new tab | pass |
| phone-fullscreen-iframe | the game keeps running and the controller still works | pass |
| phone-fullscreen-iframe | page logged no errors | pass |
| phone-small | first visit: the home screen counts down on a phone, then the demo disc runs | pass |
| phone-small | the controller is on screen, laid out inside the window, and the guest is analog | pass |
| phone-small | every pad button reaches guest port 1 with the right PS1 bit | pass |
| phone-small | the D-pad registers all four directions | pass |
| phone-small | the left stick drags to an analog axis and recentres on release | pass |
| phone-small | the menu button opens the touch menu (paused), and again closes it (resumed) | pass |
| phone-small | Reset from the touch menu restarts in place (no menu, no download) | pass |
| phone-small | Start over on the game's case from the touch menu restarts it without a download | pass |
| phone-small | rotating keeps the game running and re-lays the controller out | pass |
| phone-small | backgrounding and returning releases held buttons and keeps the game running | pass |
| phone-small | the fullscreen button does not break the player | pass |
| phone-small | reload comes back to the home screen; tapping Start disc plays | pass |
| phone-small | page logged no errors | pass |

## The shell

`src/ui/menu/home/` is the one shell, before and during a game. It is a view over `MenuState`: the
same library, settings and game rows, the same `MenuInput` edges from keyboard, pad and touch, the
same `MenuAction`s going out. Tokens (colours, type scale, spacing, easings) live in `tokens.rs`.

- Sections, in one ring: Home, Game (only while a game is started), Settings, Controls. L1/R1
  (Q/E, PageUp/PageDown, a sideways swipe) cycle them from anywhere and wrap. Back (Circle, Esc, G)
  goes to Home from any section, and from Home resumes the game. Escape, Select+Start and the touch
  Menu button are Back before a start and on a section: they never start a disc.
- Home is the library: big cases on the floor, or a grid (the button at the top, Triangle or Tab).
  L2/R2 jump by first letter with a big letter badge; on a phone a rail at the edge scrubs the
  letters. Choosing a disc plays the launch sequence (2.3 s of wall clock at any frame rate; the boot starts with it; any input fast-forwards it on its own curves instead of jumping to the end).
- On a phone the sections are a floating bar at the bottom, clear of the touch overlay, centred in the
  viewport with the same margin either side (also inside another page's frame).
- Debug is not a section: it is a live panel of the game view (`ui/debug_sidebar.rs`). A window wider than
  tall docks it on the right (a phone held sideways gives it about 38% of the width and the controller lays
  itself out in what is left); a taller one docks it as a strip between the game and the controller's panel.
  The game never pauses for it. Toolbar bug button, F3, the touch Debug button and Select+R3 on a pad toggle it.
  On a phone its header is one line of small type (a status dot, fps, ms, % of real time, the disc name only
  when it fits, close), the tabs are small, and the content gets the rest. In portrait the panel is a sheet:
  it opens fully up over the controller's panel (the interface row stays under the picture, the pad's controls
  are away), and its grab bar drags it down to a short strip with the controller back above it, or up again;
  a tap on the bar flips it. A chart that a finger starts on passes vertical travel up to the panel's scroll
  and scrubs only on a sideways drag.
- The first run opens on the "What is this?" pages with the first-visit countdown held behind them (their taps
  and keys do not cancel it); closing them, by Done, Skip or Back, remembers that they were seen and starts the
  countdown. Journeys: `*-explainer` (first run, all three pages, closing, reload, a visitor from before).
- The demo disc's seven games are chips under its title on Home (`home.contents` in the page probe).
- Menu sounds (`ui_sound.rs`): synthesised, quiet, played only while a menu is on screen; the player counts what
  it asked for (`sounds` in `psoxideUxState()`), which is what the `*-sounds` journeys read.
- The interaction matrix (`phone-matrix-<auto|on|off>-<portrait|landscape>`): sequences across the toolbar, the
  touch controls, the debug sidebar, the menu, pause, a physical pad and the orientation, with a model checking
  every visible piece after every step.
- Scrolling feels like iOS everywhere the shell scrolls (`ui/momentum.rs`): the content follows the finger,
  coasts on after it lifts (velocity of the last 100 ms, 0.998 per ms deceleration), stretches against the ends
  with rising resistance and springs back without crossing them, and a finger put down on a coasting list only
  stops it (it presses nothing). Sliders, row taps and sideways swipes are decided as before. Settings, Game,
  Controls, the save-state sheet and the debug panel use it; the wheel and a trackpad move the same positions
  (a wheel step ends a coast, and the ends never stretch for it).
- Save states are a sheet of slots (`ui/menu/slots.rs`, `app/slots.rs`, `psoxide-settings/src/slots.rs`):
  the quick slot and eight numbered ones, each with a picture, the time and how far the game had run, in
  Save and Load modes. A slot is the machine state plus a trailer (thumbnail, disc hash, format) kept in one
  file (desktop) or one IndexedDB record (web), so a save made on another disc, another game or another state
  format is listed as such and refused with a message, the game untouched. F5 and F7 (Select+R1 and Select+L1
  on a pad) quick-save and quick-load; the toolbar and the touch Save and Load buttons open the sheet.
- The touch controller never draws over the game picture: in portrait the interface buttons are the first
  line of the panel under it; in landscape every control lives in the black bar on its side of the picture.
- Controls is drawn by the shell itself (`home/controls.rs`): live device cards (grey while idle,
  cyan while a device is in use) with a port chip and an analog toggle, the remappable keyboard map
  (select a cap, press the new key; Escape cancels; Reset), and the touch rows with a layout preview.
  A wide window shows the three groups side by side; a phone shows one under a Devices, Keyboard,
  Touch bar. Every control is a focus stop for pad, keys and touch. The page probe `psoxideUxState().home` lists the page, the focus, the cases and every
  hit area by name, which is how the journeys tap and click.
- Box art: xlenore/psx-covers by boot serial (read from SYSTEM.CNF), then libretro-thumbnails by file
  name, fetched at run time only and cached (IndexedDB on the web, a cache folder on the desktop);
  Settings has a switch. Live check: `node tools/web-journeys/run.mjs --site <dir> --scenario cors`.

Journeys for the debug panel, save states and centring: `desktop-debug`, `phone-*-debug` (the panel opens, the
game keeps its speed, the controller is laid out around it, nothing meets the game picture), `desktop-slots`,
`phone-*-slots` (save, overwrite with a question, load, quick keys, reload and load again, a save from another disc or
a newer format refused, a damaged save refused, delete) and `desktop-centre`, `phone-*-centre` (the Controls
touch preview and the phone tab bar are centred). `phone-momentum*` and `desktop-momentum` drive the lists
with real touch and wheel events (a flick coasts and slows, a touch stops it and presses no row, the stretch
at both ends, a slider drag leaves the list alone); the `-debug` journeys also check the one-line header, the
portrait sheet and its handle, and flings, tap-to-stop and stretch inside the panel.

Journeys: `desktop-home`, `phone-home`, `phone-home-small`, `phone-home-landscape` (first visit, the
countdown cancelled by each input, return visit, sections, L1/R1, ways back, welcome pages, settings
controls, layout inside the safe area), `desktop-shell`, `phone-shell`, `phone-shell-small`,
`phone-shell-landscape` (the launch sequence to the boot, the shell over a running game, every section
by every way back) `desktop-covers` (a hit, a miss, offline, cached, switched off, real recents), `desktop-controls`,
`desktop-controls-narrow`, `phone-controls`, `phone-controls-small`, `phone-controls-landscape` (the
three groups fit with 44 px targets and nothing overlapping, a device's port and analog toggle, rebinding
a key and Reset, keyboard focus along the map, the touch rows) and `desktop-throttled`, `phone-throttled`
(animation frames held to 4 a second: nav clicks, keys, taps and a 70 ms pad press are never lost, and the
launch sequence takes about 4.6 s of wall clock, not of frames).
Scenarios: `--scenario launch --throttle N` (cost of the sequence), `shots` (before/after screens),
`bench`, `cors`.

## Parity audit: the new shell against the live v0.48-web3 (ba8b07b4)

| web3 feature | New shell | Evidence |
|---|---|---|
| Demo disc boots at once on a first visit | Home opens at once, the case, a 3 s countdown line, then the launch sequence and the boot | desktop-direct, desktop-home |
| Later visits | Stay on Home until Start disc (web3 booted again) | desktop-direct reload, phone reload |
| Esc / Select+Start / touch Menu pause and resume the same machine | Same; before a start and on a section they are Back and never launch | desktop-direct, desktop-home, desktop-shell |
| Back (Circle, G) closes the menu and resumes | Back on Home resumes; Back on a section returns Home | desktop-direct, desktop-shell |
| Menu rows: Resume, Save state, Load state, Reset, Record input from boot, Load input replay, Replay last (native) | Game section (same rows, same actions) | desktop-menus |
| Save-states panel (list, pin, confirm) | Same panel, still a window over the shell (not restyled) | desktop-menus |
| Reset restarts in place, no menu, no download | Same (toolbar, Game / Reset) | desktop-direct |
| Pick the loaded disc again restarts, no re-download | Start over on the game's case | desktop-direct, phone-portrait |
| Pick another disc | Shelf: every game of the folder is a case; Play | desktop-covers |
| Homebrew list, SDK examples (native) | Shelf cases; unbuilt ones offer Build examples | native only, not browser-tested |
| Open a games folder | The folder case | desktop-covers |
| Reopen your games folder (only after one was connected) | A "Reopen your games folder" case | desktop-covers |
| Recent discs (native, opened by hand) | Recent cases, real play history only; the web keeps its own in localStorage | desktop-covers, unit tests |
| Open disc..., Choose games folder, Refresh library (native) | Footer links and the folder case | native only |
| Burn menu (native) | Burn to CD under the focused case | native only, not tested |
| Settings: Video scale, Texture filter, Deinterlace, Volume, Mute, Menu opacity, UI scale, slow-host choice, Touch controls/opacity/size/haptics/left-handed | Settings section: sliders, toggles, choices, steppers | desktop-menus (every row changes), desktop-home |
| Fullscreen row, button, iPhone fallback | Settings row, touch button, same fallback | phone-fullscreen* |
| About | Welcome pages (version, licence, links) | desktop-home |
| Quit (native) | Settings row | native only |
| Controls panel (ports, drawing, rebinding) | Controls section inside the shell: device cards, key map, touch rows and preview, tabs on a phone | desktop-controls, phone-controls*, desktop-menus, *-shell |
| Toolbar: pause, reset, save, controls, volume, mute, debug, collapse | Unchanged; controls opens the Controls section, debug the sidebar | desktop-menus |
| Debug panels: guest performance, CPU, memory, VRAM, input tape, developer | Debug section (tabs, pinned summary), the sidebar beside the game on desktop | desktop-menus, *-shell |
| Touch controller, analog default, sticks, D-pad | Unchanged; Pause, Save, Debug and Menu form one pill | phone-portrait, phone-landscape |
| No WebGPU message, settings persistence, embed player | Unchanged | desktop-nowebgpu, desktop-iframe |
| New | Box art (xlenore by serial, libretro by name, switch in Settings), L1/R1 and swipe through sections, compact grid, letter jump | desktop-covers, desktop-home |

Gaps left: the save-states panel and the burn dialog are still egui windows over the shell; the editor build's Editor column has no nav entry yet; the phone Debug is a full page, not a sheet over the running game.

## Shell polish journeys (10-24 night)

Added after the first drive-through: phone-home checks that no touch controller is drawn on the home
screen; phone-shell checks that the on-screen menu button answers mouse clicks (resume, then pause into
the shell) as well as taps; the controls journeys check that binding a key another action holds swaps the
two and says so; the shell journeys sample the screen's mean brightness through the boot flash (limit 0.3);
the pad-button step tells a stalled page (no frames drawn, retried once, logged) from a missed hit. The
`--scenario review --label <name> --shots <dir>` run takes the before/after screenshots at 1280x800,
800x600, 393x852, 360x640 and 852x393.

## Polish round 2 (10-25)

- A folder is a list of games. The desktop and the browser use one grouping
  (`psoxide_settings::game_group`): everything a cue sheet or playlist names is part of it, only a
  disc's data track is shown when its `(Track N)` files stand alone, and `(Disc N)` discs of one
  game are one case. The browser's folder scan reads cue and m3u names (the first 64 KB of each)
  and opens the data track; it has no multi-track audio yet.
- Box art is a chain, tried in order: the disc's own `<name>.cover.png`/`cover.png` (desktop), the
  boot serial in xlenore, the Redump list's name for the serial in libretro-thumbnails, the CRC-32
  of the data track in the list (then its serial in xlenore and its name in libretro), and the
  file name last. The list (`libretro-database`, CC BY-SA 4.0) is downloaded once and cached, not
  bundled; hashing is off the UI thread, one disc at a time, cached by file, size and time.
- A case whose picture is being looked up shimmers and shows a thin turning ring (after a short
  delay, so a cached picture never flashes it); the picture fades in over the placeholder; a miss
  settles back without a flicker. The probe's `cases` entries carry `[title, line, cover state,
  focus, loading, art fade]`.
- The focused case lifts toward you, turns slowly, stands in teal light and gets brackets that ease
  in; the others settle back (a spring per case). Settings > Reduce motion (or the browser's
  setting) keeps only a scale and a glow.
- The launch is 2.3 s: the disc and the tray share one tilted frame, so the disc ends flat in the
  recess; discs are drawn 96-sided with label and hub rings; the spin goes on while the game loads.
- Journeys added: `*-grouping`, `*-cover-states`, `*-selection`; the launch ones now expect 2.3 s.

## Hints, analog and the toolbar (10-25, round 2)

- Every control that has a button shows it. `home/hints.rs` draws the four face glyphs (cross,
  circle, triangle, square) and the L1/R1/L2/R2 pills; a controller shows glyphs, a desktop
  keyboard shows key caps (Enter, Esc, Tab, H, Q, E), a touch screen shows none. Where: Triangle
  beside the view switch; L1 and R1 at the two ends of the tab bar (top nav and phone bar, clickable
  and lit for a moment by the press); Cross on Start/Play; Square beside What's on it, Start over and
  Burn (Square is the focused case's second action); L2/R2 "A to Z" when the shelf has letters; a
  row at the foot of Settings, Game, Controls, Debug and Welcome; "Any button skips" under the launch.
- Analog: a controller that reports sticks starts in analog mode, and the player's choice is saved
  by controller name (`input.analog_choice`). The keyboard is never analog: no toggle on its card, no
  Analog or stick-key rows in its map, its mode key does nothing, and `set_profile` refuses it. It
  presents a DualShock that is digital until a game asks, so the demo disc still reaches analog by
  its own handshake. (A game asking is not a setting.)
- The toolbar over a game, the touch buttons around it, the toast and the debug panels use the
  shell's palette and states (`theme.rs` palette, `toolbar.rs` shell style).
- Journeys added: `*-hints`; the Controls device step now expects no keyboard toggle and an analog
  controller.

## Motion and honesty pass (10-25, round 3)

- The tab highlight is one pill (phone bar) or one diamond (desktop top nav) that slides to the
  page you pick on a small spring (`home/motion.rs`: about 8 percent overshoot, settles in under half a
  second). The Controls selector (Devices, Keyboard, Touch) is the same pill in a segmented bar. Reduced
  motion snaps every spring. The probe carries `home.nav_hl`, `home.nav_vis` and `home.controls.seg`.
- A page arrives part by part: the title, then each row or card in turn, sliding in along the way the
  nav went (`motion::stage`, one clock per page: `home.page_age`). The Home text re-arrives when another
  case takes focus. List pages have one focus frame that glides between rows. Anything pressed glows
  under the finger while held (`home.press`), except cases, sliders and the letter rail.
- Numbers (the slider values, the Debug summary and plot readouts, the toolbar FPS) are set in the
  shell's own type. Only the CPU and Memory hex dumps keep the pixel face, where columns must line up.
- A phone is not told it has a keyboard: `InputRouter::keyboard_present` is true on a desktop and, on a
  device whose primary pointer is a finger, only after a key has been pressed. The card reads "Not
  connected" and the probe's `home.keyboard` says which.
- Over a paused game the touch controller's own pause, save, debug, menu and fullscreen buttons are
  not laid out (`TouchDevice::set_shell_up`); the shell header carries a close button (hit `close`) that
  resumes the game. The scrim over the game is about 84 percent. The running game is the first case on
  Home with a RUNNING chip, Resume as the button and Start over under it.
- Inside another page's frame (itch.io) the phone tab bar leaves the bottom right corner clear for the
  host's own fullscreen button (`home.framed`).
- Journeys added: `*-polish`, `*-polish-375`, `*-polish-small`, `*-polish-landscape`, `*-polish-iframe`,
  `desktop-polish`, `desktop-polish-small`.
