#!/usr/bin/env node
// Browser journey tests for the web player (docs/web-player-journeys.md).
//
// No dependencies: Node 22 and a Chrome. It serves the player (a trunk `dist`
// merged with a disc delivery) itself, with Range support, launches headless
// Chrome, drives it over the DevTools protocol with real (trusted) input events,
// and reads the player's state through `psoxideUxState()` / `psoxideTouchState()`.
//
//   node tools/web-journeys/run.mjs --site <dir> [--only <substring>] [--shots <dir>]
//        [--chrome <path>] [--report <file.md>]
//
// `--site` is a directory holding index.html, the wasm and the disc delivery
// (demo-disc.pxi / .pxw / web-manifest.txt). Exit status is 1 when any step fails.

import { spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]?.startsWith("--") ? "1" : (all[i + 1] ?? "1")]);
    return acc;
  }, []),
);
const SITE = path.resolve(args.site ?? "site");
const SHOTS = args.shots ? path.resolve(args.shots) : null;
const ONLY = args.only ?? "";
const CHROME =
  args.chrome ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
// Ports default to a spread keyed by the process id, so two runs (two lanes) do not collide.
const PORT = Number(args.port ?? 20000 + (process.pid % 20000) * 2);
const DEBUG_PORT = Number(args["debug-port"] ?? (PORT < 40000 ? PORT + 20000 : PORT - 20000));
if (SHOTS) fs.mkdirSync(SHOTS, { recursive: true });

// ---------------------------------------------------------------- servers

const MIME = {
  ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm",
  ".png": "image/png", ".json": "application/json", ".txt": "text/plain",
};

/** The request log of the site server: what the page asked for, in order. */
const requests = [];

function serveSite() {
  return http.createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    const file = path.join(SITE, decodeURIComponent(url.pathname === "/" ? "/index.html" : url.pathname));
    if (!file.startsWith(SITE) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      res.writeHead(404).end("not found");
      return;
    }
    const size = fs.statSync(file).size;
    const range = /bytes=(\d*)-(\d*)/.exec(req.headers.range ?? "");
    requests.push({ path: url.pathname, range: range ? `${range[1]}-${range[2]}` : null, t: Date.now() });
    const headers = {
      "Content-Type": MIME[path.extname(file)] ?? "application/octet-stream",
      "Cache-Control": "no-store",
      "Accept-Ranges": "bytes",
      "Access-Control-Allow-Origin": "*",
    };
    if (range) {
      const start = range[1] === "" ? size - Number(range[2]) : Number(range[1]);
      const end = range[1] !== "" && range[2] !== "" ? Math.min(Number(range[2]), size - 1) : size - 1;
      res.writeHead(206, { ...headers, "Content-Range": `bytes ${start}-${end}/${size}`, "Content-Length": end - start + 1 });
      fs.createReadStream(file, { start, end }).pipe(res);
    } else {
      res.writeHead(200, { ...headers, "Content-Length": size });
      fs.createReadStream(file).pipe(res);
    }
  });
}

/** A parent page on another origin, embedding the player like itch.io does. */
function serveParent() {
  return http.createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    const query = url.search ? url.search + "&" : "?";
    res.writeHead(200, { "Content-Type": "text/html" });
    res.end(`<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width, initial-scale=1"><style>html,body{margin:0;height:100%;background:#222}iframe{border:0;width:100%;height:100%}</style>
<iframe id=f allow="autoplay; fullscreen; gamepad; accelerometer; gyroscope" src="http://127.0.0.1:${PORT}/index.html${url.search}"></iframe>`);
  });
}

// ---------------------------------------------------------------- DevTools

class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.handlers = [];
    ws.onmessage = (m) => {
      const msg = JSON.parse(m.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        msg.error ? reject(new Error(`${msg.error.message}`)) : resolve(msg.result);
      } else if (msg.method) {
        for (const h of this.handlers) h(msg);
      }
    };
  }
  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((ok, bad) => { ws.onopen = ok; ws.onerror = bad; });
    return new Cdp(ws);
  }
  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    this.ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  on(handler) { this.handlers.push(handler); }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A fake Gamepad (installed in every frame) that a step can press. */
const FAKE_GAMEPAD = `(() => {
  const pad = { id: "Xbox 360 Controller (STANDARD GAMEPAD Vendor: 045e Product: 028e)", index: 0, connected: true,
    mapping: "standard", timestamp: 0, axes: [0,0,0,0], buttons: Array.from({length: 17}, () => ({pressed:false, touched:false, value:0})) };
  window.__fakePad = { pad, on: false };
  const real = navigator.getGamepads ? navigator.getGamepads.bind(navigator) : () => [];
  navigator.getGamepads = () => (window.__fakePad.on ? [pad, null, null, null] : real());
  window.__fakePadSet = (buttons, axes) => {
    pad.timestamp = performance.now();
    pad.buttons.forEach((b, i) => { const p = !!(buttons && buttons[i]); b.pressed = p; b.touched = p; b.value = p ? 1 : 0; });
    if (axes) pad.axes = axes;
  };
  window.__fakePadConnect = () => {
    window.__fakePad.on = true;
    const ev = new Event("gamepadconnected"); ev.gamepad = pad;
    window.dispatchEvent(ev);
  };
})();`;

class Page {
  /** `top` is the page session; `app` the session the player runs in (the iframe's, when embedded). */
  constructor(cdp, top, label) {
    this.cdp = cdp;
    this.top = top;
    this.app = top;
    this.label = label;
    this.logs = [];
    this.frames = new Map();
    cdp.on((msg) => {
      if (msg.method === "Runtime.consoleAPICalled" && ["error", "warning"].includes(msg.params.type)) {
        this.logs.push(`${msg.params.type}: ${msg.params.args.map((a) => a.value ?? a.description).join(" ")}`);
      }
      if (msg.method === "Runtime.exceptionThrown") {
        this.logs.push(`exception: ${msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text}`);
      }
      if (msg.method === "Target.attachedToTarget" && msg.params.targetInfo.type === "iframe") {
        this.attachFrame(msg.params.sessionId);
      }
    });
  }

  async attachFrame(session) {
    const send = (m, p) => this.cdp.send(m, p, session);
    await send("Page.enable"); await send("Runtime.enable");
    await send("Page.addScriptToEvaluateOnNewDocument", { source: FAKE_GAMEPAD });
    for (const source of this.initScripts ?? []) await send("Page.addScriptToEvaluateOnNewDocument", { source });
    await this.applyEmulation(session);
    await send("Runtime.runIfWaitingForDebugger").catch(() => {});
    this.app = session;
    this.frameSessions = [...(this.frameSessions ?? []), session];
  }

  async setup() {
    const s = this.top;
    await this.cdp.send("Page.enable", {}, s);
    await this.cdp.send("Runtime.enable", {}, s);
    await this.cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: FAKE_GAMEPAD }, s);
    await this.cdp.send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, s);
  }

  /** Run `source` in every new document of the page, iframes included (call before goto). */
  async addInit(source) {
    this.initScripts = [...(this.initScripts ?? []), source];
    await this.cdp.send("Page.addScriptToEvaluateOnNewDocument", { source }, this.top);
  }

  async goto(url) {
    this.app = this.top;
    this.frameSessions = [];
    await this.cdp.send("Page.navigate", { url }, this.top);
    await sleep(300);
  }

  async eval(expression, session = this.app) {
    const r = await this.cdp.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }, session);
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
    return r.result.value;
  }

  /** The player's transport: running, menu_open, loaded_id, machine, port1_mode, touch layout. */
  async ux() {
    try {
      const text = await this.eval(`(async () => { const m = await import(window.psoxideWasm.js); return m.psoxideUxState(); })()`);
      return text ? JSON.parse(text) : null;
    } catch { return null; }
  }
  async touchState() {
    try {
      return JSON.parse(await this.eval(`(async () => { const m = await import(window.psoxideWasm.js); return m.psoxideTouchState(); })()`));
    } catch { return null; }
  }
  async bench() {
    try {
      return JSON.parse(await this.eval(`(async () => { const m = await import(window.psoxideWasm.js); return m.psoxideBenchStats(); })()`));
    } catch { return null; }
  }

  async waitFor(what, predicate, timeoutMs = 60000) {
    const start = Date.now();
    let last = null;
    while (Date.now() - start < timeoutMs) {
      last = await this.ux();
      if (last && (await predicate(last))) return last;
      await sleep(150);
    }
    await this.shot(`timeout-${this.label}`);
    throw new Error(`timeout waiting for ${what}; last state ${JSON.stringify(last)?.slice(0, 400)}; logs ${this.logs.slice(0, 3).join(" | ")}`);
  }

  async shot(name) {
    if (!SHOTS) return;
    const r = await this.cdp.send("Page.captureScreenshot", { format: "png" }, this.top);
    fs.writeFileSync(path.join(SHOTS, `${name}.png`), Buffer.from(r.data, "base64"));
  }

  /** Bytes of a low-quality JPEG of the page: a black or empty frame is a few KB, a drawn scene is tens of KB. */
  async frameWeight() {
    const r = await this.cdp.send("Page.captureScreenshot", { format: "jpeg", quality: 40 }, this.top);
    return Buffer.from(r.data, "base64").length;
  }

  async firstDrawnFrame(timeoutMs) {
    const start = Date.now();
    let bytes = 0;
    while (Date.now() - start < timeoutMs) {
      bytes = await this.frameWeight();
      if (bytes >= BLANK_JPEG_BYTES) return { ok: true, ms: Date.now() - start, bytes };
      await sleep(250);
    }
    return { ok: false, ms: Date.now() - start, bytes };
  }

  async applyEmulation(session) {
    const { width, height, dpr, mobile } = this.size;
    // Device metrics reach cross-origin iframes through the top-level target; only the top accepts the command.
    if (session === this.top) await this.cdp.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: dpr, mobile }, session);
    await this.cdp.send("Emulation.setTouchEmulationEnabled", mobile ? { enabled: true, maxTouchPoints: 5 } : { enabled: false }, session).catch(() => {});
    // A notch and a home indicator, like an iPhone: env(safe-area-inset-*) reads these.
    const insets = !mobile ? { top: 0, bottom: 0, left: 0, right: 0 }
      : width < height ? { top: 47, bottom: 34, left: 0, right: 0 } : { top: 0, bottom: 21, left: 47, right: 47 };
    await this.cdp.send("Emulation.setSafeAreaInsetsOverride", { insets }, session).catch(() => {});
  }

  async viewport(width, height, { dpr = 1, mobile = false } = {}) {
    this.size = { width, height, dpr, mobile };
    await this.applyEmulation(this.top);
    // A cross-origin iframe is its own target and needs the same emulation (pointer type, ratio).
    for (const session of this.frameSessions ?? []) await this.applyEmulation(session).catch(() => {});
  }
  async throttle(rate) { await this.cdp.send("Emulation.setCPUThrottlingRate", { rate }, this.top); }

  // Real input, delivered to the top page (the iframe fills it, so coordinates agree).
  async key(key, down = true, up = true) {
    const KEYS = {
      Escape: ["Escape", 27], Enter: ["Enter", 13], Backspace: ["Backspace", 8],
      ArrowUp: ["ArrowUp", 38], ArrowDown: ["ArrowDown", 40], ArrowLeft: ["ArrowLeft", 37], ArrowRight: ["ArrowRight", 39],
      F9: ["F9", 120], F5: ["F5", 116], F7: ["F7", 118],
    };
    const [code, vk] = KEYS[key] ?? [key.length === 1 ? `Key${key.toUpperCase()}` : key, key.toUpperCase().charCodeAt(0)];
    const base = { key: key.length === 1 ? key : key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk };
    if (down) await this.cdp.send("Input.dispatchKeyEvent", { type: "rawKeyDown", ...base }, this.top);
    if (up) await this.cdp.send("Input.dispatchKeyEvent", { type: "keyUp", ...base }, this.top);
  }
  async tapKey(key, holdMs = 90) {
    await this.key(key, true, false);
    await sleep(holdMs);
    await this.key(key, false, true);
    await sleep(90);
    if (args.trace) {
      const u = await this.ux();
      if (args.trace === "shots") await this.shot(`trace-${String((this.traceN = (this.traceN ?? 0) + 1)).padStart(3, "0")}-${key}`);
      console.log(`      key ${key} -> menu_open=${u?.menu_open} running=${u?.running} panel=${u?.panel} ${u?.menu?.column}/${u?.menu?.row} machine=${u?.machine}`);
    }
  }
  async mouse(type, x, y) {
    await this.cdp.send("Input.dispatchMouseEvent", { type, x, y, button: type === "mouseMoved" ? "none" : "left", buttons: type === "mouseReleased" ? 0 : 1, clickCount: 1 }, this.top);
  }
  async click(x, y, holdMs = 70) {
    await this.mouse("mouseMoved", x, y);
    await sleep(40);
    await this.mouse("mousePressed", x, y);
    await sleep(holdMs);
    await this.mouse("mouseReleased", x, y);
    await sleep(120);
    // Park the pointer in a corner: a menu row passing under it would steal the selection.
    await this.mouse("mouseMoved", 2, (this.size?.height ?? 800) - 2);
    await sleep(60);
  }
  async touch(type, points) {
    await this.cdp.send("Input.dispatchTouchEvent", { type, touchPoints: points.map(([x, y, id = 1]) => ({ x, y, id, radiusX: 8, radiusY: 8, force: 1 })) }, this.app);
  }
  async tap(x, y, holdMs = 90) {
    await this.touch("touchStart", [[x, y]]);
    await sleep(holdMs);
    await this.touch("touchEnd", []);
    await sleep(150);
  }
  async padSet(buttons, axes) {
    await this.eval(`window.__fakePadSet(${JSON.stringify(buttons)}, ${JSON.stringify(axes ?? null)})`);
  }
  async padConnect() {
    await this.eval(`window.__fakePadConnect()`);
  }
}

// ---------------------------------------------------------------- harness

const results = [];
let current = null;

function check(name, ok, detail = "") {
  results.push({ journey: current, name, ok: !!ok, detail: ok ? "" : String(detail) });
  console.log(`${ok ? "PASS" : "FAIL"}  ${current} / ${name}${ok ? "" : "  " + detail}`);
  return !!ok;
}
/** Run a step; an exception is a failed step, not a crash. */
async function step(name, fn) {
  try {
    const r = await fn();
    if (r === undefined || r === true) check(name, true);
    else check(name, false, typeof r === "string" ? r : JSON.stringify(r));
  } catch (e) {
    check(name, false, e.message.slice(0, 300));
  }
  if (!results.at(-1).ok && activePage) await activePage.shot(`FAIL-${current}-${results.length}`);
}
let activePage = null;

const PS1 = {
  SELECT: 0x1, L3: 0x2, R3: 0x4, START: 0x8, UP: 0x10, RIGHT: 0x20, DOWN: 0x40, LEFT: 0x80,
  L2: 0x100, R2: 0x200, L1: 0x400, R1: 0x800, TRIANGLE: 0x1000, CIRCLE: 0x2000, CROSS: 0x4000, SQUARE: 0x8000,
};
const DEMO = "demo-disc"; // filled in from the first loaded state

let chrome, cdp, siteServer, parentServer;

async function launch(hostDpr = 1) {
  siteServer ??= serveSite().listen(PORT, "127.0.0.1");
  parentServer ??= serveParent().listen(PORT + 1, "localhost");
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "psoxide-journeys-"));
  chrome = spawn(CHROME, [
    "--headless=new", `--force-device-scale-factor=${hostDpr}`, `--remote-debugging-port=${DEBUG_PORT}`, `--user-data-dir=${profile}`,
    "--no-first-run", "--no-default-browser-check", "--enable-unsafe-webgpu",
    "--autoplay-policy=no-user-gesture-required", "--window-size=1280,800",
    "--disable-background-timer-throttling", "--disable-renderer-backgrounding", "--disable-backgrounding-occluded-windows",
    "about:blank",
  ], { stdio: "ignore" });
  for (let i = 0; i < 60; i++) {
    try {
      const v = await (await fetch(`http://127.0.0.1:${DEBUG_PORT}/json/version`)).json();
      cdp = await Cdp.connect(v.webSocketDebuggerUrl);
      return profile;
    } catch { await sleep(250); }
  }
  throw new Error("Chrome did not start");
}

async function newPage(label) {
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  const page = new Page(cdp, sessionId, label);
  page.targetId = targetId;
  activePage = page;
  await page.setup();
  return page;
}

const direct = (q = "") => `http://127.0.0.1:${PORT}/index.html${q}`;
const embedded = (q = "") => `http://localhost:${PORT + 1}/${q}`;

// ---------------------------------------------------------------- journeys

/** Desktop page open, tap/click to play is not needed: the demo disc boots itself. */
async function waitDemoRunning(page, timeout = 90000) {
  return page.waitFor("the demo disc running", (u) => u.running && !u.menu_open && u.loaded_id && u.port1_mode !== "none", timeout);
}

/**
 * The pad negotiation (profile applied to the new machine, the guest's configuration commands) settles
 * a frame or two after a boot, so a read taken at that instant can still say "digital". Poll until the
 * port reads analog, bounded; returns the last mode seen.
 */
async function settledMode(page, timeoutMs = 10000) {
  const start = Date.now();
  let mode = null;
  while (Date.now() - start < timeoutMs) {
    mode = (await page.ux())?.port1_mode ?? null;
    if (mode === "analog") break;
    await sleep(150);
  }
  return mode;
}

const nth = (list, i) => list[i];

async function toolbarButtons(page) {
  // The toolbar is a row of icon buttons right-aligned in the top bar. Find them from
  // a screenshot-free rule: the bar is 40 css px tall and the buttons are 34 px wide,
  // 8 px apart, ending 8 px before the right edge (see ui/toolbar.rs).
  return page.size;
}

async function journeyDesktop(label, urlFor, { embeddedMode = false } = {}) {
  current = `${label}`;
  const page = await newPage(label);
  await page.viewport(1280, 800, { dpr: 1 });
  requests.length = 0;
  await page.goto(urlFor());
  let u;
  await step("first visit: the demo disc boots by itself and runs", async () => {
    u = await waitDemoRunning(page);
  });
  if (!u) { await cdp.send("Target.closeTarget", { targetId: page.targetId }); return page; }
  await page.shot(`${label}-01-first-visit`);
  const firstMachine = u.machine;
  await step("guest sees an analog DualShock with no F9 step", async () => {
    // The pad negotiation settles a frame or two after boot; wait for it, bounded.
    const settled = await page.waitFor("the analog mode", (x) => x.port1_mode === "analog", 10000).catch(() => null);
    return settled ? true : `port1_mode=${(await page.ux()).port1_mode} after 10 s`;
  });
  await step("keyboard arrow reaches guest port 1 (left stick and D-pad)", async () => {
    await page.key("ArrowRight", true, false);
    await sleep(250);
    const t = await page.touchState();
    await page.key("ArrowRight", false, true);
    return t && (t.port1_mask & PS1.RIGHT || Math.abs(t.left_x) > 0.4) ? true : JSON.stringify(t);
  });
  await step("keyboard Cross (F) reaches the guest", async () => {
    await page.key("f", true, false);
    await sleep(250);
    const t = await page.touchState();
    await page.key("f", false, true);
    return t && t.port1_mask & PS1.CROSS ? true : JSON.stringify(t);
  });

  // The PS button.
  await step("Esc opens the menu and pauses; Esc closes it and resumes the same machine", async () => {
    await page.tapKey("Escape");
    const open = await page.waitFor("menu open", (s) => s.menu_open && !s.running, 5000);
    await page.shot(`${label}-02-menu-open`);
    await page.tapKey("Escape");
    const closed = await page.waitFor("menu closed", (s) => !s.menu_open && s.running, 5000);
    return closed.machine === open.machine && closed.machine === firstMachine ? true : "machine changed";
  });
  await step("the menu does not reopen by itself", async () => {
    await sleep(2500);
    const s = await page.ux();
    return !s.menu_open && s.running ? true : JSON.stringify(s);
  });
  await step("Back (Circle, G) closes the menu and resumes the game", async () => {
    await page.tapKey("Escape");
    await page.waitFor("menu open", (s) => s.menu_open && !s.running, 5000);
    await page.tapKey("g");
    const s = await page.waitFor("resumed", (x) => !x.menu_open && x.running, 5000);
    await sleep(1500);
    const later = await page.ux();
    return later.running && !later.menu_open && s.machine === firstMachine ? true : JSON.stringify(later);
  });

  // Reset from the toolbar.
  requests.length = 0;
  await step("toolbar Reset restarts the game in place: no menu, no download", async () => {
    const before = await page.ux();
    await page.click(1280 - RESET_FROM_RIGHT, 20);
    const s = await page.waitFor("a restarted machine", (x) => x.machine > before.machine && x.running && !x.menu_open, 15000);
    await sleep(1200);
    const later = await page.ux();
    const refetched = requests.filter((r) => /web-manifest|\.pxi$/.test(r.path));
    await page.shot(`${label}-03-after-reset`);
    const drawn = await page.firstDrawnFrame(20000);
    console.log(`      first drawn frame ${drawn.ms} ms after the reset settled (${drawn.bytes} bytes)`);
    if (!drawn.ok) return `the screen is still blank ${drawn.ms} ms after reset (${drawn.bytes} bytes)`;
    if (!(later.running && !later.menu_open)) return `after reset: ${JSON.stringify(later)}`;
    if (refetched.length) return `Reset fetched ${refetched.map((r) => r.path)}`;
    if (later.loaded_id !== before.loaded_id) return "game identity changed";
    return (await settledMode(page)) === "analog" ? true : `port1_mode=${s.port1_mode}`;
  });

  // Reset from the menu: Esc, Right to the Game column, Down to Reset, Enter.
  await step("menu Reset restarts the game in place and closes the menu", async () => {
    const before = await page.ux();
    await page.tapKey("Escape");
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await menuSelect(page, "Game", "Reset");
    const s = await page.waitFor("a restarted machine", (x) => x.machine > before.machine && x.running && !x.menu_open, 15000);
    await sleep(1500);
    const later = await page.ux();
    return later.running && !later.menu_open ? true : JSON.stringify(later);
  });

  // Pick the loaded disc again.
  requests.length = 0;
  await step("picking the loaded disc again restarts it with no download", async () => {
    const before = await page.ux();
    await page.tapKey("Escape");
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await menuSelect(page, "Library", "Homebrew");
    await page.shot(`${label}-04-library`);
    await menuSelect(page, "Library", "PSoXide Demo Disc");
    const s = await page.waitFor("a restarted machine", (x) => x.machine > before.machine && x.running && !x.menu_open, 20000);
    await sleep(1500);
    const later = await page.ux();
    const heavy = requests.filter((r) => /\.pxw$/.test(r.path) && r.range && /^\d+-\d+$/.test(r.range) && Number(r.range.split("-")[1]) - Number(r.range.split("-")[0]) > 1_000_000);
    if (!(later.running && !later.menu_open)) return `after pick: ${JSON.stringify(later)}`;
    return heavy.length ? `big disc reads: ${heavy.length}` : true;
  });

  await step("a toolbar pause stays paused when a menu opened over it is closed", async () => {
    await page.click(1280 - PAUSE_FROM_RIGHT, 20);
    const paused = await page.waitFor("paused", (x) => !x.running && !x.menu_open, 5000);
    await page.tapKey("Escape");
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await page.tapKey("g");
    await sleep(500);
    const s = await page.ux();
    if (s.menu_open || s.running) return `expected paused with no menu: ${JSON.stringify(s)}`;
    await page.click(1280 - PAUSE_FROM_RIGHT, 20);
    await page.waitFor("running again", (x) => x.running, 5000);
  });

  await step("hiding and showing the tab leaves the game running with the menu closed", async () => {
    await page.eval(`Object.defineProperty(document, "hidden", {value: true, configurable: true}); Object.defineProperty(document, "visibilityState", {value: "hidden", configurable: true}); document.dispatchEvent(new Event("visibilitychange"));`);
    await sleep(500);
    await page.eval(`Object.defineProperty(document, "hidden", {value: false, configurable: true}); Object.defineProperty(document, "visibilityState", {value: "visible", configurable: true}); document.dispatchEvent(new Event("visibilitychange"));`);
    await sleep(1500);
    const s = await page.ux();
    return s.running && !s.menu_open ? true : JSON.stringify(s);
  });

  await step("resizing the window (rotate) keeps the game and the menu state", async () => {
    await page.viewport(800, 1280, { dpr: 1 });
    await sleep(800);
    await page.shot(`${label}-05-portrait`);
    await page.viewport(1280, 800, { dpr: 1 });
    await sleep(800);
    const s = await page.ux();
    return s.running && !s.menu_open ? true : JSON.stringify(s);
  });

  await step("a pad connected later is analog and drives the guest", async () => {
    await page.padConnect();
    await sleep(500);
    await page.padSet({ 0: true }); // A = Cross in the standard mapping
    await sleep(500);
    const t = await page.touchState();
    const s = await page.ux();
    await page.padSet({});
    return (await settledMode(page)) === "analog" && t.port1_mask & PS1.CROSS ? true : JSON.stringify({ t, mode: s.port1_mode });
  });

  await step("reload: the player comes back running with the menu closed", async () => {
    await page.goto(urlFor());
    const s = await waitDemoRunning(page);
    return (await settledMode(page)) === "analog" ? true : s.port1_mode;
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}


/**
 * Every drawn control (pad buttons and the menu / fullscreen buttons) lies inside the window minus the
 * safe-area insets, and no two pills overlap. `t` is the layout probe: rects are [mask, x0, y0, x1, y1]
 * in window pixels, safe is [left, top, right, bottom].
 */
function layoutProblems(t, size, dpr) {
  const [sl, st, sr, sb] = t.safe;
  const W = size.w * dpr, H = size.h * dpr;
  const eps = 0.6;
  for (const [id, x0, y0, x1, y1] of t.rects) {
    if (x0 < sl - eps || y0 < st - eps || x1 > W - sr + eps || y1 > H - sb + eps) {
      return `control ${id ? "0x" + id.toString(16) : "ui"} [${x0}, ${y0}, ${x1}, ${y1}] is outside the safe area [${sl}, ${st}, ${W - sr}, ${H - sb}]`;
    }
  }
  for (let i = 0; i < t.rects.length; i++) {
    for (let j = i + 1; j < t.rects.length; j++) {
      const [, a0, b0, a1, b1] = t.rects[i], [, c0, d0, c1, d1] = t.rects[j];
      const overlap = Math.min(a1, c1) - Math.max(a0, c0) > 1 && Math.min(b1, d1) - Math.max(b0, d0) > 1;
      const faces = t.rects[i][0] >= 0x1000 && t.rects[j][0] >= 0x1000; // face buttons overlap by design
      if (overlap && !faces) return `controls ${t.rects[i][0]} and ${t.rects[j][0]} overlap`;
    }
  }
  const inset = t.safe.some((v) => v > 0);
  return inset ? null : "the safe-area insets did not reach the player (env() read as zero)";
}

/** Phone journeys: the on-screen controller, the touch menu, rotation. */
async function journeyPhone(label, urlFor, size) {
  current = label;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile: true });
  requests.length = 0;
  await page.goto(urlFor());
  const dpr = size.dpr;
  let u;
  await step("first visit: the tap-to-play splash shows, then the demo disc runs", async () => {
    await page.waitFor("the emulator loaded", (s) => s.has_bus, 90000);
    const splash = await page.eval(`document.getElementById("psoxide-tap")?.classList.contains("is-on")`);
    await page.shot(`${label}-01-splash`);
    if (!splash) return "no tap-to-play splash on a touch device";
    await page.tap(size.w / 2, size.h / 2);
    await sleep(500);
    const gone = await page.eval(`!document.getElementById("psoxide-tap")?.classList.contains("is-on")`);
    if (!gone) return "the splash did not go away after a tap";
    u = await waitDemoRunning(page);
  });
  if (!u) { await cdp.send("Target.closeTarget", { targetId: page.targetId }); return; }
  await sleep(1500);
  await page.shot(`${label}-02-playing`);
  const layout = async () => (await page.ux()).touch;
  if (args.trace) console.log("      frame", await page.eval(`JSON.stringify({w: innerWidth, h: innerHeight, dpr: devicePixelRatio, cw: document.querySelector("canvas")?.width, coarse: matchMedia("(pointer: coarse)").matches, top: window.top === window})`), JSON.stringify((await page.ux()).touch?.buttons?.slice(0, 2)));
  await step("the controller is on screen, laid out inside the window, and the guest is analog", async () => {
    const s = await page.ux();
    const t = s.touch;
    if (!t) return "no touch layout (controller hidden)";
    if ((await settledMode(page)) !== "analog") return `port1_mode=${s.port1_mode}`;
    const bad = t.buttons.filter(([, x, y]) => x < 0 || y < 0 || x > size.w * dpr || y > size.h * dpr);
    if (bad.length) return `buttons outside the window: ${JSON.stringify(bad)}`;
    if (t.buttons.length !== 10) return `expected 10 pad buttons, got ${t.buttons.length}`;
    return layoutProblems(t, size, dpr) ?? true;
  });
  if (args.trace) {
    await page.eval(`window.__ev = []; for (const t of ["pointerdown","touchstart","mousedown","click"]) window.addEventListener(t, (e) => window.__ev.push(t + ":" + Math.round(e.clientX ?? e.touches?.[0]?.clientX) + "," + Math.round(e.clientY ?? e.touches?.[0]?.clientY) + ":" + e.target.tagName), true)`);
    await page.tap(size.w / 2, size.h * 0.7);
    console.log("      events seen in the frame:", await page.eval(`JSON.stringify(window.__ev)`));
  }
  await step("every pad button reaches guest port 1 with the right PS1 bit", async () => {
    const t = await layout();
    const wrong = [];
    for (const [mask, x, y] of t.buttons) {
      await page.touch("touchStart", [[x / dpr, y / dpr]]);
      let st = null;
      for (let k = 0; k < 8; k++) {
        await sleep(120);
        st = await page.touchState();
        if (st.port1_mask & mask) break;
      }
      await page.touch("touchEnd", []);
      await sleep(120);
      if (args.trace) console.log("      touch state", JSON.stringify(st), x / dpr, y / dpr);
      if (!(st.port1_mask & mask)) wrong.push(`0x${mask.toString(16)}:${st.port1_mask.toString(16)}`);
    }
    return wrong.length ? `buttons that did not register: ${wrong.join(" ")}` : true;
  });
  await step("the D-pad registers all four directions", async () => {
    const t = await layout();
    const [cx, cy, half] = t.dpad;
    const arm = half * 0.7;
    const wrong = [];
    for (const [name, dx, dy, mask] of [["up", 0, -arm, PS1.UP], ["down", 0, arm, PS1.DOWN], ["left", -arm, 0, PS1.LEFT], ["right", arm, 0, PS1.RIGHT]]) {
      await page.touch("touchStart", [[(cx + dx) / dpr, (cy + dy) / dpr]]);
      await sleep(160);
      const st = await page.touchState();
      await page.touch("touchEnd", []);
      await sleep(120);
      if (!(st.port1_mask & mask)) wrong.push(`${name}:${st.port1_mask.toString(16)}`);
    }
    return wrong.length ? `directions that did not register: ${wrong.join(" ")}` : true;
  });
  await step("the left stick drags to an analog axis and recentres on release", async () => {
    const t = await layout();
    const left = t.sticks.find((s) => s[0] === "L");
    if (!left) return "no left stick in analog mode";
    const [, rx, ry, ring] = left;
    await page.touch("touchStart", [[rx / dpr, ry / dpr]]);
    await sleep(80);
    await page.touch("touchMove", [[(rx + ring * 0.9) / dpr, ry / dpr]]);
    await sleep(250);
    const st = await page.touchState();
    await page.touch("touchEnd", []);
    await sleep(250);
    const after = await page.touchState();
    if (!(st.left_x > 0.5)) return `stick right gave left_x=${st.left_x}`;
    return Math.abs(after.left_x) < 0.1 ? true : `stick did not recentre: ${after.left_x}`;
  });
  await step("the menu button opens the touch menu (paused), and again closes it (resumed)", async () => {
    const t = await layout();
    const menu = t.ui.find((b) => b[0] === "menu");
    const before = await page.ux();
    await page.tap(menu[1] / dpr, menu[2] / dpr);
    await page.waitFor("menu open", (s) => s.menu_open && !s.running, 5000);
    await page.shot(`${label}-03-menu`);
    const open = await page.ux();
    const menu2 = open.touch?.ui.find((b) => b[0] === "menu");
    if (!menu2) return "the menu button vanished while the menu is open";
    await page.tap(menu2[1] / dpr, menu2[2] / dpr);
    const s = await page.waitFor("menu closed", (x) => !x.menu_open && x.running, 5000);
    return s.machine === before.machine ? true : "machine changed";
  });
  await step("Reset from the touch menu restarts in place (no menu, no download)", async () => {
    if (!(size.w === 393 && size.h === 852)) return true; // row positions are calibrated for 393x852
    const before = await page.ux();
    const menu = before.touch.ui.find((b) => b[0] === "menu");
    requests.length = 0;
    await page.tap(menu[1] / dpr, menu[2] / dpr);
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await touchMenuSelect(page, size, "Game", "Reset");
    const s = await page.waitFor("a restarted machine", (x) => x.machine > before.machine && x.running && !x.menu_open, 20000);
    await sleep(1000);
    const later = await page.ux();
    const refetched = requests.filter((r) => /web-manifest|\.pxi$/.test(r.path));
    await page.shot(`${label}-04-after-reset`);
    if (refetched.length) return `Reset fetched ${refetched.map((r) => r.path)}`;
    return later.running && !later.menu_open && (await settledMode(page)) === "analog" ? true : JSON.stringify(later);
  });
  await step("picking the loaded disc from the touch menu restarts it without a download", async () => {
    if (!(size.w === 393 && size.h === 852)) return true; // row positions are calibrated for 393x852
    const before = await page.ux();
    const menu = before.touch.ui.find((b) => b[0] === "menu");
    requests.length = 0;
    await page.tap(menu[1] / dpr, menu[2] / dpr);
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await touchMenuSelect(page, size, "Library", "Homebrew");
    await touchMenuSelect(page, size, "Library", "PSoXide Demo Disc");
    const s = await page.waitFor("a restarted machine", (x) => x.machine > before.machine && x.running && !x.menu_open, 20000);
    await sleep(1000);
    const heavy = requests.filter((r) => /\.pxw$/.test(r.path) && r.range && Number(r.range.split("-")[1]) - Number(r.range.split("-")[0]) > 1_000_000);
    return heavy.length ? `big disc reads: ${heavy.length}` : true;
  });
  await step("rotating keeps the game running and re-lays the controller out", async () => {
    await page.viewport(size.h, size.w, { dpr: size.dpr, mobile: true });
    await sleep(1200);
    await page.shot(`${label}-05-rotated`);
    const s = await page.ux();
    const t = s.touch;
    if (!(s.running && !s.menu_open)) return JSON.stringify(s);
    if (!t) return "controller hidden after rotation";
    const bad = t.buttons.filter(([, x, y]) => x < 0 || y < 0 || x > size.h * dpr || y > size.w * dpr);
    if (bad.length) return `buttons outside the rotated window: ${JSON.stringify(bad)}`;
    // And every button still works in landscape.
    const wrong = [];
    for (const [mask, x, y] of t.buttons) {
      await page.touch("touchStart", [[x / dpr, y / dpr]]);
      let st = null;
      for (let k = 0; k < 8; k++) {
        await sleep(120);
        st = await page.touchState();
        if (st.port1_mask & mask) break;
      }
      await page.touch("touchEnd", []);
      await sleep(100);
      if (!(st.port1_mask & mask)) wrong.push(`0x${mask.toString(16)}`);
    }
    await page.viewport(size.w, size.h, { dpr: size.dpr, mobile: true });
    await sleep(800);
    return wrong.length ? `landscape buttons that did not register: ${wrong.join(" ")}` : true;
  });
  await step("backgrounding and returning releases held buttons and keeps the game running", async () => {
    const t = await layout();
    const [mask, x, y] = t.buttons[0];
    await page.touch("touchStart", [[x / dpr, y / dpr]]);
    await sleep(150);
    await page.eval(`Object.defineProperty(document, "hidden", {value: true, configurable: true}); Object.defineProperty(document, "visibilityState", {value: "hidden", configurable: true}); document.dispatchEvent(new Event("visibilitychange"));`);
    await sleep(400);
    await page.eval(`Object.defineProperty(document, "hidden", {value: false, configurable: true}); Object.defineProperty(document, "visibilityState", {value: "visible", configurable: true}); document.dispatchEvent(new Event("visibilitychange"));`);
    await sleep(800);
    const st = await page.touchState();
    await page.touch("touchEnd", []);
    const s = await page.ux();
    if (st.port1_mask & mask) return "a button stayed held after returning";
    return s.running && !s.menu_open ? true : JSON.stringify(s);
  });
  await step("the fullscreen button does not break the player", async () => {
    const t = await layout();
    const fs_ = t.ui.find((b) => b[0] === "fullscreen");
    if (!fs_) return true; // offered only where the browser has fullscreen
    await page.tap(fs_[1] / dpr, fs_[2] / dpr);
    await sleep(600);
    const s = await page.ux();
    return s.running ? true : JSON.stringify(s);
  });
  await step("reload comes back to the tap-to-play splash, then plays", async () => {
    await page.goto(urlFor());
    await page.waitFor("the emulator loaded", (s) => s.has_bus, 90000);
    await page.tap(size.w / 2, size.h / 2);
    const s = await waitDemoRunning(page);
    return (await settledMode(page)) === "analog" ? true : s.port1_mode;
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/**
 * Pick a row of the touch menu with finger taps. A phone-portrait menu (393x852) lists rows 54 pt tall
 * from y = 426 down, 56 pt apart; the column strip sits at y = 323 with the selected column centred and
 * the others 100 pt to each side (see shots/phone-portrait-03-menu.png). A tap on a row activates it.
 */
async function touchMenuSelect(page, size, column, row) {
  const COLUMNS = ["Library", "Game", "Settings"];
  const ROW_INDEX = { Reset: 3, Homebrew: 0, "PSoXide Demo Disc": 1 };
  const u = await page.ux();
  const here = COLUMNS.indexOf(u.menu.column);
  const want = COLUMNS.indexOf(column);
  if (here !== want) {
    await page.tap(size.w / 2 + (want - here) * 100, TOUCH_MENU_TABS_Y);
    await page.waitFor(`column ${column}`, (s) => s.menu.column === column, 3000);
  }
  await page.tap(size.w / 2, 426 + ROW_INDEX[row] * 56);
}
const TOUCH_MENU_TABS_Y = 323;


/** Every menu row and every toolbar button on the desktop layout. */
async function journeyMenus(label, urlFor) {
  current = label;
  const page = await newPage(label);
  await page.viewport(1280, 800, { dpr: 1 });
  await page.cdp.send("Page.setInterceptFileChooserDialog", { enabled: true }, page.top);
  await page.goto(urlFor());
  let u;
  await step("the player is up", async () => { u = await waitDemoRunning(page); });
  if (!u) { await cdp.send("Target.closeTarget", { targetId: page.targetId }); return; }
  const open = async () => { await page.tapKey("Escape"); await page.waitFor("menu open", (s) => s.menu_open, 5000); };
  const settle = async () => { await sleep(250); };
  const rowValue = async (column, row) => {
    await menuSelect(page, column, row, { enter: false });
    return (await page.ux()).menu.value;
  };

  // Game column.
  await open();
  await step("Game / Resume (Enter) resumes the game and closes the menu", async () => {
    await menuSelect(page, "Game", "Resume");
    const s = await page.waitFor("running", (x) => x.running && !x.menu_open, 5000);
    return s.loaded_id ? true : "no game";
  });
  await open();
  if (!(args.skipstep ?? "").split(",").includes("save")) await step("Game / Save state stores a browser quick-save", async () => {
    await menuSelect(page, "Game", "Save state");
    const s = await page.waitFor("save status", (x) => /quick-save/i.test(x.status), 8000);
    return true;
  });
  await step("Game / Load state opens the save-states panel", async () => {
    await menuSelect(page, "Game", "Load state");
    await page.waitFor("save-states panel", (x) => x.panel === "savestates", 4000);
    await page.shot(`${label}-10-savestates`);
  });
  await step("the save-states panel closes with Back and leaves the menu usable", async () => {
    await page.tapKey("g");
    await page.tapKey("Escape");
    await sleep(300);
    let s = await page.ux();
    if (s.panel !== "none") { await page.tapKey("Escape"); s = await page.ux(); }
    if (s.panel !== "none") return `panel still ${s.panel}`;
    if (!s.menu_open) await open();
  });
  if (!(args.skipstep ?? "").split(",").includes("record")) await step("Game / Record input starts a recording (the game reboots); the same row stops it", async () => {
    const before = await page.ux();
    await menuSelect(page, "Game", "Record input");
    await page.waitFor("rebooted for the recording", (x) => x.machine > before.machine && x.running && !x.menu_open, 15000);
    await sleep(800);
    await open();
    await menuSelect(page, "Game", "Stop recording");
    await sleep(600);
    const s = await page.ux();
    return s.has_bus ? true : "machine lost";
  });
  await sleep(300);
  let st = await page.ux();
  if (!st.menu_open) await open();
  if (!(args.skipstep ?? "").split(",").includes("replay")) await step("Game / Load input replay does not break the player", async () => {
    await menuSelect(page, "Game", "Load input replay");
    await sleep(500);
    st = await page.ux();
    return st.has_bus ? true : "machine lost";
  });
  await step("no control appears twice, and the Library offers no Reconnect on a fresh visit", async () => {
    if (!(await page.ux()).menu_open) await open();
    const seen = new Map();
    const labels = {};
    for (const column of ["Library", "Game", "Settings"]) {
      await menuSelect(page, column, "", { enter: false }).catch(() => {});
      const rows = (await page.ux()).menu.rows;
      labels[column] = [];
      for (let i = 0; i < rows; i++) {
        const u = await page.ux();
        labels[column].push(u.menu.row);
        await page.tapKey("ArrowDown");
      }
    }
    for (const [column, list] of Object.entries(labels)) {
      for (const label of list) {
        const key = label.replace(/\s*\(F\d\)$/, "").toLowerCase();
        if (seen.has(key)) return `"${label}" is in both ${seen.get(key)} and ${column}`;
        seen.set(key, column);
      }
    }
    const all = Object.values(labels).flat().join(" | ");
    if (/reconnect|reopen/i.test(all)) return `a saved-folder row shows on a fresh visit: ${all}`;
    console.log(`      menu rows: ${JSON.stringify(labels)}`);
  });

  // Settings column: every cycling row changes its value.
  const SWEEP = ["Video scale", "Texture filter", "Deinterlace", "Volume", "Mute", "Menu opacity", "UI scale", "If the device is too slow", "Touch controls", "Touch opacity", "Touch size", "Touch haptics", "Left-handed layout"];
  for (const row of SWEEP.filter((r) => !(args.skip ?? "").split(",").includes(r))) {
    await step(`Settings / ${row} changes its value`, async () => {
      if (!(await page.ux()).menu_open) await open();
      const before = await rowValue("Settings", row);
      await page.tapKey("Enter");
      await settle();
      const after = (await page.ux()).menu.value;
      return after !== before ? true : `value stayed "${before}"`;
    });
  }
  await step("Settings / UI scale back to 100% (so the later steps aim right)", async () => {
    for (let i = 0; i < 8; i++) {
      const v = await rowValue("Settings", "UI scale");
      if (v === "100%") return true;
      await page.tapKey("Enter");
      await settle();
    }
    return "never came back to 100%";
  });
  await step("Settings / Touch controls cycles back to Auto (desktop shows no pad)", async () => {
    for (let i = 0; i < 6; i++) {
      if (await rowValue("Settings", "Touch controls") === "Auto") return true;
      await page.tapKey("Enter");
      await settle();
    }
    return "never came back to Auto";
  });
  await step("Settings / Controls opens the controls panel; Esc closes it", async () => {
    if (!(await page.ux()).menu_open) await open();
    await menuSelect(page, "Settings", "Controls");
    await page.waitFor("controls panel", (x) => x.panel === "controls", 4000);
    await page.shot(`${label}-11-controls`);
    await page.tapKey("Escape");
    await page.waitFor("panel closed", (x) => x.panel === "none", 4000);
  });
  await step("Settings / About opens the card; Enter closes it", async () => {
    let s = await page.ux();
    if (!s.menu_open) await open();
    await menuSelect(page, "Settings", "About");
    await page.waitFor("about card", (x) => x.panel === "about", 4000);
    await page.shot(`${label}-12-about`);
    await page.tapKey("Enter");
    await page.waitFor("card closed", (x) => x.panel === "none", 4000);
  });
  await step("Library rows: Homebrew expands, the folder rows are present, and closing resumes", async () => {
    await menuSelect(page, "Library", "Homebrew");
    await menuSelect(page, "Library", "PSoXide Demo Disc", { enter: false });
    await menuSelect(page, "Library", "Open a games folder", { enter: false });
    await page.tapKey("Escape");
    await page.waitFor("resumed", (x) => x.running && !x.menu_open, 5000);
  });

  // Toolbar (desktop): pause, reset, save, controls, volume, mute, debug, collapse.
  const bar = (fromRight) => 1280 - fromRight;
  await step("toolbar: save icon opens the save-states panel", async () => {
    await page.click(964, 17);
    await page.waitFor("save-states panel", (x) => x.panel === "savestates", 4000);
    await page.tapKey("Escape");
    await sleep(300);
    const s = await page.ux();
    if (s.panel !== "none") { await page.click(964, 17); await sleep(300); }
  });
  await step("toolbar: controller icon opens the controls panel", async () => {
    let s = await page.ux();
    if (s.panel !== "none") { await page.tapKey("Escape"); await sleep(300); }
    await page.click(1004, 17);
    await page.waitFor("controls panel", (x) => x.panel === "controls", 4000);
    await page.tapKey("Escape");
    await page.waitFor("panel closed", (x) => x.panel === "none", 4000);
  });
  await step("toolbar: pause stops the guest and play resumes it", async () => {
    let s = await page.ux();
    if (!s.running || s.menu_open) await page.tapKey("Escape");
    await page.click(876, 17);
    await page.waitFor("paused", (x) => !x.running, 4000);
    await page.click(876, 17);
    await page.waitFor("running", (x) => x.running, 4000);
  });
  await step("toolbar: the volume slider and the mute icon change Settings / Volume and Mute", async () => {
    await page.tapKey("Escape"); await page.waitFor("menu", (x) => x.menu_open, 4000);
    const vol0 = await rowValue("Settings", "Volume");
    const mute0 = await rowValue("Settings", "Mute");
    await page.tapKey("Escape"); await page.waitFor("closed", (x) => !x.menu_open, 4000);
    await page.mouse("mouseMoved", 1060, 17); await page.mouse("mousePressed", 1060, 17); await sleep(60);
    await page.mouse("mouseMoved", 1090, 17); await sleep(60); await page.mouse("mouseReleased", 1090, 17); await sleep(200);
    await page.click(1160, 17);
    await page.tapKey("Escape"); await page.waitFor("menu", (x) => x.menu_open, 4000);
    const vol1 = await rowValue("Settings", "Volume");
    const mute1 = await rowValue("Settings", "Mute");
    await page.tapKey("Escape"); await page.waitFor("closed", (x) => !x.menu_open, 4000);
    if (vol0 === vol1) return `volume stayed ${vol0}`;
    return mute0 !== mute1 ? true : `mute stayed ${mute0}`;
  });
  await step("toolbar: debug and collapse buttons keep the game running", async () => {
    await page.click(1208, 17);
    await sleep(500);
    await page.shot(`${label}-13-debug`);
    await page.click(1208, 17);
    await page.click(1256, 17);
    await sleep(500);
    await page.shot(`${label}-14-collapsed`);
    const s = await page.ux();
    return s.running ? true : JSON.stringify(s);
  });
  await step("Settings / Fullscreen does not break the player", async () => {
    await page.tapKey("Escape"); await page.waitFor("menu", (x) => x.menu_open, 4000);
    await menuSelect(page, "Settings", "Fullscreen");
    await sleep(500);
    return (await page.ux()).has_bus ? true : "machine lost";
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}


/** A gamepad (the standard mapping, faked): analog from the first poll, menu chord, navigation, Reset. */
async function journeyGamepad(label, urlFor) {
  current = label;
  const page = await newPage(label);
  await page.viewport(1280, 800, { dpr: 1 });
  await page.goto(urlFor());
  let u;
  await step("the player is up", async () => { u = await waitDemoRunning(page); });
  if (!u) { await cdp.send("Target.closeTarget", { targetId: page.targetId }); return; }
  const hold = async (buttons, axes, ms = 160) => { await page.padSet(buttons, axes); await sleep(ms); await page.padSet({}, [0, 0, 0, 0]); await sleep(160); };
  await step("a pad connects as an analog DualShock and its buttons reach the guest", async () => {
    await page.padConnect();
    await sleep(400);
    await page.padSet({ 0: true }); // A = Cross
    await sleep(300);
    const t = await page.touchState();
    const s = await page.ux();
    await page.padSet({});
    await sleep(200);
    return (await settledMode(page)) === "analog" && t.port1_mask & PS1.CROSS ? true : JSON.stringify({ mode: s.port1_mode, t });
  });
  await step("the left stick reaches the guest as an axis", async () => {
    await page.padSet({}, [0.9, 0, 0, 0]);
    await sleep(300);
    const t = await page.touchState();
    await page.padSet({}, [0, 0, 0, 0]);
    await sleep(200);
    return t.left_x > 0.5 ? true : `left_x=${t.left_x}`;
  });
  await step("Select+Start opens the menu; again closes it and resumes", async () => {
    await hold({ 8: true, 9: true }, null, 250);
    await page.waitFor("menu open", (s) => s.menu_open && !s.running, 5000);
    await hold({ 8: true, 9: true }, null, 250);
    await page.waitFor("resumed", (s) => !s.menu_open && s.running, 5000);
  });
  await step("pad navigation: D-pad moves the selection, Circle closes the menu and resumes", async () => {
    await hold({ 8: true, 9: true }, null, 250);
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    const before = (await page.ux()).menu;
    await hold({ 13: true }); // D-pad down
    const after = (await page.ux()).menu;
    if (before.row === after.row) return `selection stayed on ${before.row}`;
    await hold({ 1: true }); // B = Circle = back
    await page.waitFor("resumed", (s) => !s.menu_open && s.running, 5000);
  });
  await step("pad navigation: Cross on Game / Reset restarts the game in place", async () => {
    const before = await page.ux();
    await hold({ 8: true, 9: true }, null, 250);
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    for (let i = 0; i < 8 && (await page.ux()).menu.column !== "Game"; i++) await hold({ 15: true }); // right
    for (let i = 0; i < 12 && !(await page.ux()).menu.row.includes("Reset"); i++) await hold({ 13: true });
    await hold({ 0: true }); // Cross
    await page.waitFor("a restarted machine", (s) => s.machine > before.machine && s.running && !s.menu_open, 15000);
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** A browser without WebGPU gets a plain explanation, not a panic. */
async function journeyNoWebgpu(label, urlFor) {
  current = label;
  const page = await newPage(label);
  await page.viewport(1280, 800, { dpr: 1 });
  await page.cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: `Object.defineProperty(navigator, "gpu", { value: undefined, configurable: true });` }, page.top);
  await page.goto(urlFor());
  await step("the page says it needs WebGPU and shows no fatal box", async () => {
    await sleep(2500);
    const text = await page.eval(`JSON.stringify({ err: document.getElementById("psoxide-loader")?.classList.contains("has-error"), status: document.getElementById("psoxide-loader-status")?.textContent, fatal: !!document.getElementById("psoxide-fatal") })`);
    const t = JSON.parse(text);
    await page.shot(`${label}-01`);
    return t.err && /WebGPU/.test(t.status) && !t.fatal ? true : text;
  });
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** An iPhone-like browser: no Fullscreen API. The button must still do something real. */
async function journeyFullscreenFallback(label, urlFor, { framed }) {
  current = label;
  const size = { w: 393, h: 852, dpr: 3 };
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile: true });
  await page.addInit(`
    for (const name of ["fullscreenEnabled", "webkitFullscreenEnabled", "fullscreenElement", "webkitFullscreenElement"]) {
      try { Object.defineProperty(Document.prototype, name, { get: () => (name.endsWith("Element") ? null : false), configurable: true }); } catch (e) {}
    }
    for (const name of ["requestFullscreen", "webkitRequestFullscreen"]) { try { Element.prototype[name] = undefined; } catch (e) {} }
    window.__opened = [];
    window.open = (url) => { window.__opened.push(String(url)); return {}; };
    Object.defineProperty(navigator, "standalone", { value: false, configurable: true });
  `);
  await page.goto(urlFor());
  await page.waitFor("the emulator loaded", (s) => s.has_bus, 90000);
  await page.tap(size.w / 2, size.h / 2);
  await waitDemoRunning(page);
  await sleep(1000);
  await step("the fullscreen button is offered even without the Fullscreen API", async () => {
    const t = (await page.ux()).touch;
    return t?.ui.some((b) => b[0] === "fullscreen") ? true : JSON.stringify(t?.ui);
  });
  await step(framed ? "tapping it inside a frame opens the player's own page in a new tab" : "tapping it on a page of its own explains Add to Home Screen", async () => {
    const [, x, y] = (await page.ux()).touch.ui.find((b) => b[0] === "fullscreen");
    if (args.trace) console.log("      fullscreen button at", x / size.dpr, y / size.dpr, await page.eval(`JSON.stringify({ fn: typeof window.psoxideToggleFullscreen, opened: window.__opened, top: window.top === window, fse: document.fullscreenEnabled, w: innerWidth, h: innerHeight, dpr: devicePixelRatio, cw: document.querySelector("canvas")?.width })`));
    await page.tap(x / size.dpr, y / size.dpr);
    let r;
    for (let i = 0; i < 20; i++) {
      await sleep(250);
      r = JSON.parse(await page.eval(`JSON.stringify({ opened: window.__opened, tip: document.getElementById("psoxide-tip").classList.contains("is-on"), text: document.getElementById("psoxide-tip-text").textContent })`));
      if (r.opened.length || r.tip) break;
    }
    await page.shot(`${label}-01`);
    if (framed) return r.opened.length === 1 && /#fs$/.test(r.opened[0]) && !/embed/.test(r.opened[0]) ? true : JSON.stringify(r);
    return r.tip && /Home Screen/.test(r.text) && r.opened.length === 0 ? true : JSON.stringify(r);
  });
  await step("the game keeps running and the controller still works", async () => {
    const s = await page.ux();
    return s.running && !s.menu_open ? true : JSON.stringify(s);
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

// Toolbar geometry (css px from the right edge, centre of the button), measured on a
// 1280-wide window in the first run; see docs/web-player-journeys.md.
// A drawn launcher frame is well above this at 1280x800 and quality 40; black is a few KB.
const BLANK_JPEG_BYTES = 12000;
const RESET_FROM_RIGHT = 364;
const PAUSE_FROM_RIGHT = 404;

/**
 * Walk the open menu with the keyboard to a column and a row (by label substring), reading
 * the selection back from the state probe. Enter on the row when `enter` is set.
 */
async function menuSelect(page, column, row, { enter = true } = {}) {
  const COLUMNS = ["Library", "Game", "Settings"];
  let last = null;
  for (let i = 0; i < 60; i++) {
    const u = await page.ux();
    const here = u.menu.column;
    if (last && last.menu.column !== here && last.pressed === "ArrowDown") {
      console.log(`      NOTE menu column jumped ${last.menu.column} -> ${here} on ArrowDown (row ${last.menu.row}); before ${JSON.stringify({...last, touch: 0})} after ${JSON.stringify({...u, touch: 0})}`);
    }
    if (here !== column) {
      const delta = (COLUMNS.indexOf(column) - COLUMNS.indexOf(here) + 3) % 3;
      last = { ...u, pressed: delta === 1 ? "ArrowRight" : "ArrowLeft" };
      await page.tapKey(last.pressed);
    } else if (!u.menu.row.includes(row)) {
      last = { ...u, pressed: "ArrowDown" };
      await page.tapKey("ArrowDown");
    } else {
      if (enter) await page.tapKey("Enter");
      return;
    }
  }
  throw new Error(`could not reach ${column} / ${row}: ${JSON.stringify((await page.ux()).menu)}`);
}

// ---------------------------------------------------------------- main

/** Run `fn` against a fresh Chrome whose display has device pixel ratio `hostDpr` (the
 * emulated device ratio must match it, or winit's pixel size and the browser disagree). */
async function inChrome(hostDpr, fn) {
  const profile = await launch(hostDpr);
  try {
    await fn();
  } finally {
    try { await cdp.send("Browser.close"); } catch {}
    chrome.kill();
    await sleep(500);
    try { fs.rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 300 }); } catch {}
  }
}

async function scenarioSpam() {
  current = "spam";
  const page = await newPage("spam");
  await page.viewport(1280, 800, { dpr: 1 });
  await page.goto(direct());
  await waitDemoRunning(page);
  await page.tapKey("Escape");
  await page.waitFor("menu", (s) => s.menu_open, 5000);
  await menuSelect(page, "Settings", "Controls", { enter: false });
  let prev = (await page.ux()).menu;
  for (let i = 0; i < 40; i++) {
    const k = i % 4 === 0 ? "Enter" : "ArrowDown";
    await page.tapKey(k);
    const u = await page.ux();
    if (args.trace) console.log(`  ${k} -> ${u.menu.column}/${u.menu.row}=${u.menu.value} status=${u.status}`);
    if (u.menu.column !== prev.column) console.log(`jump at press ${i}: ${prev.column}/${prev.row} -> ${u.menu.column}/${u.menu.row}`);
    prev = u.menu;
  }
  console.log("done spam");
}

/** Emulated speed of the web build: `--scenario bench --query "?disc=games/quake.bin" --seconds 20 --throttle 4`. */
async function scenarioBench() {
  current = "bench";
  const page = await newPage("bench");
  await page.viewport(Number(args.width ?? 1280), Number(args.height ?? 800), { dpr: 1 });
  if (args.throttle) await page.throttle(Number(args.throttle));
  await page.goto(`http://127.0.0.1:${PORT}/index.html${args.query ?? ""}`);
  // Not the UX probe: this must also run against a build that predates it.
  for (let i = 0; i < 800; i++) {
    const b = await page.bench();
    if (b && b.guest_frames > 300) break;
    await sleep(150);
  }
  await sleep(4000);
  // `--keys f:2500,f:2500` taps keys with a pause after each (e.g. Cross through a menu into a level).
  for (const spec of (args.keys ?? "").split(",").filter(Boolean)) {
    const [key, wait] = spec.split(":");
    await page.tapKey(key, 120);
    await sleep(Number(wait ?? 1500));
  }
  const warm = Number(args.warmup ?? 8) * 1000;
  await sleep(warm);
  await page.eval(`(async () => { const m = await import(window.psoxideWasm.js); m.psoxideBenchReset(); })()`);
  const t0 = Date.now();
  await sleep(Number(args.seconds ?? 20) * 1000);
  const st = await page.bench();
  const secs = (Date.now() - t0) / 1000;
  const per = (x) => (st.guest_frames ? (x / st.guest_frames).toFixed(2) : "n/a");
  const out = {
    label: args.label ?? "", throttle: Number(args.throttle ?? 1), seconds: +secs.toFixed(1),
    guest_fps: +(st.guest_frames / secs).toFixed(1), redraws_per_s: +(st.redraws / secs).toFixed(1),
    emu_ms_per_guest_frame: per(st.emu_ms), total_ms_per_guest_frame: per(st.total_ms),
    hw_render_ms_per_guest_frame: per(st.hw_render_ms), egui_ms_per_guest_frame: per(st.egui_ms),
    max_redraw_ms: +st.max_total_ms.toFixed(1), long_redraws: st.long_redraws, audio_underrun_frames: st.audio_underrun_frames,
    disc_waits: st.disc_waits,
  };
  console.log(JSON.stringify(out));
  await page.shot(`bench-${args.label ?? "x"}`);
}

async function main() {
  if (args.scenario === "bench") { await inChrome(1, scenarioBench); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "spam") { await inChrome(1, scenarioSpam); siteServer?.close(); parentServer?.close(); process.exit(0); }
  const wanted = (name) => !ONLY || name.includes(ONLY);
  try {
    if (!ONLY || ONLY.includes("desktop") || ["gamepad", "menus", "nowebgpu"].some((k) => ONLY.includes(k))) {
      await inChrome(1, async () => {
        if (wanted("desktop-direct")) await journeyDesktop("desktop-direct", () => direct());
        if (wanted("desktop-iframe")) await journeyDesktop("desktop-iframe", () => embedded(), { embeddedMode: true });
        if (wanted("desktop-menus")) await journeyMenus("desktop-menus", () => direct());
        if (wanted("desktop-nowebgpu")) await journeyNoWebgpu("desktop-nowebgpu", () => direct());
        if (wanted("desktop-gamepad")) await journeyGamepad("desktop-gamepad", () => direct());
        if (wanted("desktop-gamepad-iframe")) await journeyGamepad("desktop-gamepad-iframe", () => embedded());
      });
    }
    if (!ONLY || ONLY.includes("phone")) {
      await inChrome(3, async () => {
        if (wanted("phone-portrait")) await journeyPhone("phone-portrait", () => direct(), { w: 393, h: 852, dpr: 3 });
        if (wanted("phone-iframe")) await journeyPhone("phone-iframe", () => embedded(), { w: 393, h: 852, dpr: 3 });
        if (wanted("phone-landscape")) await journeyPhone("phone-landscape", () => direct(), { w: 852, h: 393, dpr: 3 });
        if (wanted("phone-fullscreen")) await journeyFullscreenFallback("phone-fullscreen", () => direct(), { framed: false });
        if (wanted("phone-fullscreen-iframe")) await journeyFullscreenFallback("phone-fullscreen-iframe", () => embedded(), { framed: true });
        if (wanted("phone-small")) await journeyPhone("phone-small", () => direct(), { w: 360, h: 640, dpr: 3 });
      });
    }
  } finally {
    siteServer?.close(); parentServer?.close();
  }
  const failed = results.filter((r) => !r.ok);
  const table = ["| Journey | Step | Result |", "|---|---|---|",
    ...results.map((r) => `| ${r.journey} | ${r.name} | ${r.ok ? "pass" : "FAIL: " + r.detail.replace(/\|/g, "/")} |`)].join("\n");
  if (args.report) fs.writeFileSync(path.resolve(args.report), table + "\n");
  console.log(`\n${results.length - failed.length}/${results.length} passed`);
  process.exit(failed.length ? 1 : 0);
}

main().catch((e) => { console.error(e); try { chrome?.kill(); } catch {} process.exit(2); });
