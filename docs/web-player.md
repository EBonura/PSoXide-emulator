# Web player (embed mode)

The web build doubles as a player that another page can show in an iframe,
for example an SDK example running next to its source. Add `?embed=1` to the
page URL and name the program with `?disc=`:

```html
<iframe src="/PSoXide/player/?embed=1&disc=../examples/hello-tri.exe"
        title="hello-tri running in PSoXide"
        allow="autoplay; gamepad; fullscreen"
        loading="lazy"
        width="640" height="480" style="border: 0"></iframe>
```

- `disc` is a same-origin path relative to the player page: a PS-EXE (fetched
  whole) or a BIN (read in ranges as the game asks for sectors). Absolute URLs
  and other origins are refused.
- `gamepad` and `autoplay` are permissions-policy features; delegating them
  costs nothing and matters when the player is on another origin. Audio
  starts from the click inside the frame either way.
- `loading="lazy"` keeps offscreen frames from loading at all.

In embed mode the page shows a click-to-start poster and loads nothing else:
the wasm is fetched and the emulator started only when the poster is clicked,
which also counts as the user gesture browsers want before playing audio.
There is no menu, toolbar or splash, Escape and Select+Start do nothing, and
the demo disc never autoboots. The canvas fills the frame. Hovering the frame
shows restart and mute buttons.

## postMessage protocol

Host to player, posted to `iframe.contentWindow`:

```js
frame.contentWindow.postMessage({ type: "psoxide-command", command: "pause" }, location.origin);
```

| `command` | Effect |
|---|---|
| `pause` | Stop emulation (holds until `resume`). |
| `resume` | Undo a host `pause`. The player stays paused while its page is hidden. |
| `reset` | Cold-boot the current program again. Keeps the paused state. |
| `mute`, `unmute` | Silence or restore audio. |

The player only accepts messages whose origin matches the player and whose `source` is its parent window, whose
`type` is `"psoxide-command"`, and whose `command` is one of the above;
anything else is ignored (an unknown command logs a console warning).
Commands sent before the user clicks start are remembered: a `pause` keeps the
program paused once it loads, unless the click itself starts it (a click
always runs). `reset` before start does nothing.

Player to host, posted to `window.parent` with the same origin:

```js
{ type: "psoxide-event", event: "running" }
{ type: "psoxide-event", event: "error", message: "..." }
```

| `event` | When |
|---|---|
| `starting` | The user clicked Play; the host can unload other instances. |
| `ready` | The player page is listening for commands (poster up, nothing loaded). |
| `running` | The program is running: after it boots, and after each resume. |
| `paused` | The program stopped: host `pause`, or the page became hidden. |
| `error` | Start or load failed (`message` says why): no `?disc=`, no WebAssembly SIMD, a fetch or boot failure, or the emulator stopping. |

Tell frames apart with `event.source === frame.contentWindow`.

The player also pauses itself on `visibilitychange` to hidden and resumes when
visible again (unless the host paused it). A frame scrolled offscreen is still
"visible", so the host should pause frames it cannot see, for example with an
`IntersectionObserver`.

## Building the bundle

```sh
python3 tools/build-web-player.py --out /path/to/player
```

The output (index.html, the JS glue and wasm, `snippets/`, the favicon and
`psoxide-player-build.json`) can be served from any path. The build record
holds the emulator revision, the rustc, trunk, wasm-bindgen and wasm-opt
versions, and a sha256 per file. The script's docstring lists the toolchain
it needs.

Players start muted. Use the speaker button to enable sound. Cross-origin parent control is not supported.
