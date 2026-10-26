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
    const answer = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    // Each journey is its own browser context; closing its page must free the context too, or a long run
    // piles up wasm instances until the machine swaps and the throttled journeys miss their clicks.
    const context = method === "Target.closeTarget" ? this.contexts.get(params.targetId) : undefined;
    if (context) {
      this.contexts.delete(params.targetId);
      return answer.finally(() => this.send("Target.disposeBrowserContext", { browserContextId: context }).catch(() => {}));
    }
    return answer;
  }
  on(handler) { this.handlers.push(handler); }
  /** Browser context of each open journey page, by target id. */
  contexts = new Map();
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A fake Gamepad (installed in every frame) that a step can press. */
const FAKE_GAMEPAD = `(() => {
  const pad = { id: "Xbox 360 Controller (STANDARD GAMEPAD Vendor: 045e Product: 028e)", index: 0, connected: true,
    mapping: "standard", timestamp: 0, axes: [0,0,0,0], buttons: Array.from({length: 17}, () => ({pressed:false, touched:false, value:0})) };
  window.__fakePad = { pad, on: false };
  // A real controller on the test machine must not leak into the runs: only the fake pad is ever reported.
  // Every poll is counted: a press only exists once the page has looked at it, so a step waits on this
  // instead of a fixed sleep (a loaded machine gives the page a frame every few hundred ms).
  window.__fakePad.polls = 0;
  navigator.getGamepads = () => { window.__fakePad.polls++; return window.__fakePad.on ? [pad, null, null, null] : [null, null, null, null]; };
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
      // Only this page's own frames: pages of finished journeys must not adopt later pages' iframes.
      if (msg.method === "Target.attachedToTarget" && msg.params.targetInfo.type === "iframe" && msg.sessionId === this.top) {
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

  /** Mean brightness of the page, 0 (black) to 1 (white), from a small copy of a screenshot. */
  async meanLuma() {
    const r = await this.cdp.send("Page.captureScreenshot", { format: "jpeg", quality: 40 }, this.top);
    return this.eval(`(async () => {
      const bmp = await createImageBitmap(await (await fetch("data:image/jpeg;base64,${r.data}")).blob());
      const g = new OffscreenCanvas(64, 64).getContext("2d");
      g.drawImage(bmp, 0, 0, 64, 64);
      const d = g.getImageData(0, 0, 64, 64).data;
      let sum = 0;
      for (let i = 0; i < d.length; i += 4) sum += 0.2126 * d[i] + 0.7152 * d[i + 1] + 0.0722 * d[i + 2];
      return sum / (d.length / 4) / 255;
    })()`, this.top);
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
  /**
   * Hold requestAnimationFrame to about `fps` frames a second in every new document, like a hidden or busy
   * browser pane (call before goto). Timers and input events are untouched, as they are in such a pane.
   */
  async throttleFrames(fps) {
    const gap = Math.round(1000 / fps);
    // All callbacks asked for inside one gap run together in one frame, as a throttled browser does.
    await this.addInit("(() => { const raw = window.requestAnimationFrame.bind(window); let queue = [], timer = 0, next = 0;"
      + " window.requestAnimationFrame = (cb) => { const id = ++next; queue.push([id, cb]);"
      + " if (!timer) timer = setTimeout(() => { timer = 0; const run = queue; queue = []; raw((t) => run.forEach(([, f]) => f(t))); }, " + gap + "); return id; };"
      + " window.cancelAnimationFrame = (id) => { queue = queue.filter(([i]) => i !== id); }; })();");
  }

  // Real input, delivered to the top page (the iframe fills it, so coordinates agree).
  async key(key, down = true, up = true) {
    const KEYS = {
      Escape: ["Escape", 27], Enter: ["Enter", 13], Backspace: ["Backspace", 8],
      ArrowUp: ["ArrowUp", 38], ArrowDown: ["ArrowDown", 40], ArrowLeft: ["ArrowLeft", 37], ArrowRight: ["ArrowRight", 39],
      F9: ["F9", 120], F3: ["F3", 114], F5: ["F5", 116], F7: ["F7", 118], PageUp: ["PageUp", 33], PageDown: ["PageDown", 34],
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
  /**
   * Set the fake pad and wait until the page has polled it twice since, so the new state was really seen (a
   * press taken between two frames of a loaded machine is not dropped). Bounded: a page that stops polling
   * (a hidden tab, a pad nothing listens to) does not hang the run.
   */
  async padSet(buttons, axes) {
    const polls = () => this.eval(`window.__fakePad.polls`).catch(() => 0);
    const before = await polls();
    await this.eval(`window.__fakePadSet(${JSON.stringify(buttons)}, ${JSON.stringify(axes ?? null)})`);
    const start = Date.now();
    while (Date.now() - start < 4000 && (await polls()) < before + 3) await sleep(30);
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

// Never leave a Chrome behind: every one this run starts is killed (with its profile directory) when the
// process exits, on an error, and on a signal. The runs are silent (--mute-audio): nothing checks the
// speakers, and a stranded Chrome would otherwise keep playing the demo disc.
const startedChromes = new Map();
function reapChromes() {
  for (const [child, profile] of startedChromes) {
    try { child.kill("SIGKILL"); } catch {}
    try { fs.rmSync(profile, { recursive: true, force: true }); } catch {}
  }
  startedChromes.clear();
}
process.on("exit", reapChromes);
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) process.on(signal, () => { reapChromes(); process.exit(130); });
process.on("uncaughtException", (e) => { console.error(e); reapChromes(); process.exit(2); });
// A DevTools call to a page that has just closed is not a failed journey.
process.on("unhandledRejection", (e) => console.error("unhandled rejection (ignored):", String(e?.message ?? e)));

async function launch(hostDpr = 1) {
  siteServer ??= serveSite().listen(PORT, "127.0.0.1");
  parentServer ??= serveParent().listen(PORT + 1, "localhost");
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "psoxide-journeys-"));
  chrome = spawn(CHROME, [
    "--headless=new", `--force-device-scale-factor=${hostDpr}`, `--remote-debugging-port=${DEBUG_PORT}`, `--user-data-dir=${profile}`,
    "--no-first-run", "--no-default-browser-check", "--enable-unsafe-webgpu", "--mute-audio",
    "--autoplay-policy=no-user-gesture-required", "--window-size=1280,800",
    "--disable-background-timer-throttling", "--disable-renderer-backgrounding", "--disable-backgrounding-occluded-windows",
    "about:blank",
  ], { stdio: "ignore" });
  startedChromes.set(chrome, profile);
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
  // Each journey is its own visitor: a fresh browser context has empty storage, so the first-visit
  // countdown is the first visit unless the journey itself reloads.
  const { browserContextId } = await cdp.send("Target.createBrowserContext");
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank", browserContextId });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  const page = new Page(cdp, sessionId, label);
  page.targetId = targetId;
  cdp.contexts.set(targetId, browserContextId);
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

/** The centre of a named hit area on the home screen (css px), from the state probe. */
async function homeHit(page, name, timeout = 8000) {
  const start = Date.now();
  while (Date.now() - start < timeout) {
    const u = await page.ux();
    const hit = u?.home?.hits?.find((h) => h[0] === name);
    if (hit) {
      if (args.trace) console.log(`      hit ${name} at ${hit.slice(1)} (page ${u.home.page}, zone ${u.home.zone})`);
      return [(hit[1] + hit[3]) / 2, (hit[2] + hit[4]) / 2, u];
    }
    await sleep(120);
  }
  const u = await page.ux();
  throw new Error(`no hit area "${name}" on the home screen: ${JSON.stringify(u?.home?.hits?.map((h) => h[0]))}`);
}
async function homeClick(page, name) { const [x, y] = await homeHit(page, name); await page.click(x, y); }
async function homeTap(page, name) { const [x, y] = await homeHit(page, name); await page.tap(x, y); }

/** Wait for the home screen with the disc booted and waiting behind it, then press Start disc. */
async function startFromHome(page, timeout = 90000) {
  await page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home && s.home.page === "Home", timeout);
  await page.tapKey("Enter");
  return waitDemoRunning(page, timeout);
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

/**
 * The toolbar's icons, found in a screenshot instead of by remembered coordinates: the x of the centre of each
 * lit run along the bar, left to right. With the full bar at 1280 wide the runs are: pause, reset, save, load,
 * controls, the volume slider, mute, debug, collapse.
 */
async function toolbarIcons(page) {
  const r = await page.cdp.send("Page.captureScreenshot", { format: "png" }, page.top);
  return page.eval(`(async () => {
    const bmp = await createImageBitmap(await (await fetch("data:image/png;base64,${r.data}")).blob());
    const g = new OffscreenCanvas(bmp.width, 34).getContext("2d");
    g.drawImage(bmp, 0, 0);
    const d = g.getImageData(0, 0, bmp.width, 34).data;
    const lit = [];
    for (let x = 0; x < bmp.width; x++) {
      let n = 0;
      for (let y = 6; y < 28; y++) { const i = (y * bmp.width + x) * 4; if (d[i] + d[i + 1] + d[i + 2] > 270) n++; }
      lit.push(n > 0);
    }
    const runs = []; let start = -1, gap = 0;
    for (let x = 0; x <= lit.length; x++) {
      if (lit[x]) { if (start < 0) start = x; gap = 0; }
      else if (start >= 0 && ++gap > 6) { runs.push([start, x - gap]); start = -1; }
    }
    return runs.filter(([a, b]) => b - a >= 6 && a > bmp.width * 0.4).map(([a, b]) => (a + b) / 2 / (bmp.width / innerWidth));
  })()`, page.top);
}

async function journeyDesktop(label, urlFor, { embeddedMode = false } = {}) {
  current = `${label}`;
  const page = await newPage(label);
  await page.viewport(1280, 800, { dpr: 1 });
  requests.length = 0;
  await page.goto(urlFor());
  let u;
  await step("first visit: the home screen counts down, then the demo disc runs", async () => {
    await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
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
  await step("picking the loaded disc again (Start over on its case) restarts it with no download", async () => {
    const before = await page.ux();
    await page.tapKey("Escape");
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await page.shot(`${label}-04-home-in-game`);
    await homeClick(page, "restart");
    const s = await page.waitFor("a restarted machine", (x) => x.machine > before.machine && x.running && !x.menu_open, 40000);
    await sleep(1500);
    const later = await page.ux();
    const heavy = requests.filter((r) => /\.pxw$/.test(r.path) && r.range && /^\d+-\d+$/.test(r.range) && Number(r.range.split("-")[1]) - Number(r.range.split("-")[0]) > 1_000_000);
    if (!(later.running && !later.menu_open)) return `after pick: ${JSON.stringify(later)}`;
    // The shell stays up over the booting game until the launch sequence has handed over.
    await page.waitFor("the shell gone", (x) => x.home.cover === 0, 15000);
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

  await step("reload: a return visit stays on the home screen until Start disc", async () => {
    await page.goto(urlFor());
    await page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
    await sleep(5000);
    const stay = await page.ux();
    if (stay.running || !stay.menu_open || stay.home.countdown !== "off") return `the return visit did not stay home: ${JSON.stringify(stay.home)}`;
    const s = await startFromHome(page);
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

/**
 * No control of the on-screen controller (pad buttons, sticks' rest points, interface buttons) meets the game
 * picture. `u` is the player's state; rects are window px and the picture css px.
 */
function gameProblems(u, dpr) {
  const g = u.game;
  const t = u.touch;
  if (!t || !g || g[2] - g[0] < 50) return "the player reports no game picture";
  for (const [id, x0, y0, x1, y1] of t.rects) {
    const r = [x0 / dpr, y0 / dpr, x1 / dpr, y1 / dpr];
    if (Math.min(r[2], g[2]) - Math.max(r[0], g[0]) > 0.75 && Math.min(r[3], g[3]) - Math.max(r[1], g[1]) > 0.75) {
      return `control ${id ? "0x" + id.toString(16) : "ui"} [${r.map((v) => v.toFixed(1))}] is over the game picture [${g}]`;
    }
  }
  for (const [name, x, y] of [...t.buttons.map((b) => [`0x${b[0].toString(16)}`, b[1], b[2]]), ...t.ui.map((b) => [b[0], b[1], b[2]])]) {
    const [cx, cy] = [x / dpr, y / dpr];
    if (cx > g[0] && cx < g[2] && cy > g[1] && cy < g[3]) return `the touch centre of ${name} (${cx.toFixed(0)},${cy.toFixed(0)}) is inside the game picture [${g}]`;
  }
  return null;
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
  await step("first visit: the home screen counts down on a phone, then the demo disc runs", async () => {
    await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
    await page.shot(`${label}-01-home`);
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
    return layoutProblems(t, size, dpr) ?? gameProblems(s, dpr) ?? true;
  });
  if (args.trace) {
    await page.eval(`window.__ev = []; for (const t of ["pointerdown","touchstart","mousedown","click"]) window.addEventListener(t, (e) => window.__ev.push(t + ":" + Math.round(e.clientX ?? e.touches?.[0]?.clientX) + "," + Math.round(e.clientY ?? e.touches?.[0]?.clientY) + ":" + e.target.tagName), true)`);
    await page.tap(size.w / 2, size.h * 0.7);
    console.log("      events seen in the frame:", await page.eval(`JSON.stringify(window.__ev)`));
  }
  await step("the canvas backing store is its CSS size times the device pixel ratio, drawn pixelated, with the picture on whole device pixels", async () => {
    const c = JSON.parse(await page.eval(`(() => { const c = document.querySelector("canvas"); const r = c.getBoundingClientRect(); return JSON.stringify({ w: c.width, h: c.height, cw: r.width * devicePixelRatio, ch: r.height * devicePixelRatio, ir: getComputedStyle(c).imageRendering, dpr: devicePixelRatio }); })()`));
    if (Math.abs(c.w - c.cw) > 1.5 || Math.abs(c.h - c.ch) > 1.5) return `the backing store is ${c.w}x${c.h} but the canvas covers ${c.cw}x${c.ch} device pixels: the browser would resample it`;
    if (!/pixelated|crisp/.test(c.ir)) return `image-rendering is "${c.ir}"`;
    const u = await page.ux();
    for (const v of u.game) if (Math.abs(v * c.dpr - Math.round(v * c.dpr)) > 0.02) return `the picture [${u.game}] is off the device pixel grid at ${c.dpr}x`;
  });
  await step("every pad button reaches guest port 1 with the right PS1 bit", async () => {
    const t = await layout();
    const wrong = [];
    // Hold a finger on a button until the guest reports it (bounded), then lift.
    const press = async (mask, x, y) => {
      await page.touch("touchStart", [[x / dpr, y / dpr]]);
      let st = null;
      for (let k = 0; k < 8; k++) {
        await sleep(120);
        st = await page.touchState();
        if (st.port1_mask & mask) break;
      }
      await page.touch("touchEnd", []);
      await sleep(120);
      return st;
    };
    /** Frames the page drew in 300 ms: a loaded or swapping machine can stall it for seconds. */
    const drawn = async () => {
      const r0 = (await page.bench())?.redraws;
      await sleep(300);
      return (await page.bench())?.redraws - r0;
    };
    for (const [mask, x, y] of t.buttons) {
      let st = await press(mask, x, y);
      if (args.trace) console.log("      touch state", JSON.stringify(st), x / dpr, y / dpr);
      if (st.port1_mask & mask) continue;
      // A miss. If the page was not drawing at all (a stall under load, seen once in a full run on a
      // busy machine), nothing was tested: wait for it to wake and press again, once. A page that is
      // drawing normally and still misses is a real bug and fails with what it saw.
      const frames = await drawn();
      // A press the page did not take, on a page that is drawing normally, is looked at again once with
      // the layout read afresh: right after a rotation the first touch can land between the old layout
      // and the new one. A second miss is a real bug and fails with what the page saw.
      if (frames >= 3) {
        await sleep(600);
        const fresh = (await layout()).buttons.find((b) => b[0] === mask);
        if (fresh) {
          console.log(`      note: 0x${mask.toString(16)} did not register at first; pressing again at its freshly read place`);
          st = await press(mask, fresh[1], fresh[2]);
          if (st.port1_mask & mask) continue;
        }
      }
      if (frames < 3) {
        console.log(`      note: the page drew ${frames} frames in 300 ms while 0x${mask.toString(16)} was pressed (stalled machine); retrying once`);
        for (let i = 0; i < 40 && (await drawn()) < 5; i++) await sleep(250);
        st = await press(mask, x, y);
        if (st.port1_mask & mask) continue;
      }
      wrong.push(`0x${mask.toString(16)}:${st.port1_mask.toString(16)} at ${Math.round(x)},${Math.round(y)} events=${st.touch_events} owned=${st.touch_owned} last=${st.last_x},${st.last_y} drawn=${frames} frames in 300ms`);
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
  await step("the menu button opens the touch menu (paused), and the shell header closes it (resumed)", async () => {
    const t = await layout();
    const menu = t.ui.find((b) => b[0] === "menu");
    const before = await page.ux();
    await page.tap(menu[1] / dpr, menu[2] / dpr);
    await page.waitFor("menu open", (s) => s.menu_open && !s.running, 5000);
    await page.shot(`${label}-03-menu`);
    const open = await page.ux();
    // The controller's own buttons step aside while the shell is up: the shell header closes it.
    if (open.touch?.ui.length) return "the controller's buttons are still over the shell";
    await homeTap(page, "close");
    const s = await page.waitFor("menu closed", (x) => !x.menu_open && x.running, 5000);
    return s.machine === before.machine ? true : "machine changed";
  });
  await step("Reset from the touch menu restarts in place (no menu, no download)", async () => {
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
  await step("Start over on the game's case from the touch menu restarts it without a download", async () => {
    const before = await page.ux();
    const menu = before.touch.ui.find((b) => b[0] === "menu");
    requests.length = 0;
    await page.tap(menu[1] / dpr, menu[2] / dpr);
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await homeTap(page, "restart");
    const s = await page.waitFor("a restarted machine", (x) => x.machine > before.machine && x.running && !x.menu_open, 40000);
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
    // And every button still works in landscape. Each press reads the layout afresh and is looked at
    // again once: right after a rotation the first touch can land between the old layout and the new.
    const wrong = [];
    const pressAt = async (mask) => {
      const now = (await layout()).buttons.find((b) => b[0] === mask);
      if (!now) return null;
      await page.touch("touchStart", [[now[1] / dpr, now[2] / dpr]]);
      let st = null;
      for (let k = 0; k < 10; k++) {
        await sleep(120);
        st = await page.touchState();
        if (st.port1_mask & mask) break;
      }
      await page.touch("touchEnd", []);
      await sleep(150);
      return st;
    };
    for (const [mask] of t.buttons) {
      let st = await pressAt(mask);
      if (!(st && st.port1_mask & mask)) {
        await sleep(500);
        st = await pressAt(mask);
      }
      if (!(st && st.port1_mask & mask)) wrong.push(`0x${mask.toString(16)}`);
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
  await step("reload comes back to the home screen; tapping Start disc plays", async () => {
    await page.goto(urlFor());
    await page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
    await sleep(3500);
    const stay = await page.ux();
    if (stay.running || stay.home.countdown !== "off") return `the return visit did not stay home: ${JSON.stringify(stay.home)}`;
    await page.shot(`${label}-06-return-home`);
    await homeTap(page, "primary");
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
  const u = await page.ux();
  if (u.home.page !== column) await homeTap(page, `nav:${column}`);
  await page.waitFor(`the ${column} page`, (s) => s.home?.page === column, 4000);
  await homeTap(page, `row:${row}`);
}

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
  await step("Game / Save state opens the slot sheet in Save mode, and Back returns to the Game page", async () => {
    await menuSelect(page, "Game", "Save state");
    const u = await page.waitFor("the slot sheet", (x) => x.home.page === "Slots" && x.panel === "savestates", 4000);
    if (u.slots.mode !== "Save") return `the sheet opened in ${u.slots.mode} mode`;
    await page.tapKey("g");
    await page.waitFor("the Game page again", (x) => x.home.page === "Game" && x.menu_open, 4000);
  });
  await step("Game / Load state opens the slot sheet in Load mode, with every slot in a grid", async () => {
    await menuSelect(page, "Game", "Load state");
    const u = await page.waitFor("the slot sheet", (x) => x.home.page === "Slots" && x.panel === "savestates", 4000);
    if (u.slots.mode !== "Load") return `the sheet opened in ${u.slots.mode} mode`;
    if (u.slots.cards.length !== 9) return `${u.slots.cards.length} slots`;
    await page.shot(`${label}-10-savestates`);
  });
  await step("the sheet closes with Back and leaves the menu usable", async () => {
    await page.tapKey("g");
    await page.waitFor("the Game page again", (x) => x.home.page === "Game" && x.menu_open && x.panel === "none", 4000);
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
    for (const column of ["Game", "Settings"]) {
      await menuSelect(page, column, "", { enter: false }).catch(() => {});
      const rows = (await page.ux()).menu.rows;
      // To the top: one more Up leaves the list for the nav row, and Down comes back.
      for (let k = 0; k < 40 && (await page.ux()).home.zone !== "nav"; k++) await page.tapKey("ArrowUp");
      await page.tapKey("ArrowDown");
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
    if (labels.Settings.includes("Controls")) return "Settings still lists Controls, which has its own tab";
    const all = Object.values(labels).flat().join(" | ");
    if (/reconnect|reopen/i.test(all)) return `a saved-folder row shows on a fresh visit: ${all}`;
    console.log(`      menu rows: ${JSON.stringify(labels)}`);
  });

  // Settings column: every cycling row changes its value.
  const SWEEP = ["Video scale", "Texture filter", "Deinterlace", "Volume", "Mute", "Menu opacity", "UI scale", "If the device is too slow", "Touch controls", "Touch opacity", "Touch size", "Touch haptics", "Left-handed layout", "Box art from the internet"];
  for (const row of SWEEP.filter((r) => !(args.skip ?? "").split(",").includes(r))) {
    await step(`Settings / ${row} changes its value`, async () => {
      if (!(await page.ux()).menu_open) await open();
      const before = await rowValue("Settings", row);
      await page.tapKey("ArrowRight");
      await settle();
      const after = (await page.ux()).menu.value;
      return after !== before ? true : `value stayed "${before}"`;
    });
  }
  await step("Settings / UI scale back to 100% (so the later steps aim right)", async () => {
    for (let i = 0; i < 8; i++) {
      const v = await rowValue("Settings", "UI scale");
      if (v === "100%") return true;
      await page.tapKey("ArrowRight");
      await settle();
    }
    return "never came back to 100%";
  });
  await step("Settings / Touch controls cycles back to Auto (desktop shows no pad)", async () => {
    for (let i = 0; i < 6; i++) {
      if (await rowValue("Settings", "Touch controls") === "Auto") return true;
      await page.tapKey("ArrowRight");
      await settle();
    }
    return "never came back to Auto";
  });
  await step("Settings / About opens the welcome pages; Next pages through them and Back closes them", async () => {
    let s = await page.ux();
    if (!s.menu_open) await open();
    await menuSelect(page, "Settings", "About");
    await page.waitFor("welcome pages", (x) => x.panel === "about", 4000);
    await page.shot(`${label}-12-about`);
    await page.tapKey("ArrowRight");
    await page.tapKey("ArrowLeft");
    await page.tapKey("g");
    await page.waitFor("welcome closed", (x) => x.panel === "none", 4000);
  });
  await step("Home shows the game's case, the demo disc and the folder case; Escape on Home resumes", async () => {
    if (!(await page.ux()).menu_open) await open();
    for (let i = 0; i < 6 && (await page.ux()).home.page !== "Home"; i++) await page.tapKey("PageDown");
    const names = (await page.ux()).home.hits.map((h) => h[0]);
    if (!names.includes("case:0") || !names.includes("primary") || !names.includes("restart")) return `Home lacks its cases: ${names}`;
    if (names.some((n) => /^nav:Library/.test(n))) return "a Library entry is still in the nav";
    await page.tapKey("Escape");
    await page.waitFor("resumed", (x) => x.running && !x.menu_open, 5000);
  });

  // Toolbar (desktop): pause, reset, save, controls, volume, mute, debug, collapse.
  const bar = (fromRight) => 1280 - fromRight;
  await step("toolbar: the save and load icons open the slot sheet in their own mode, and Back resumes the game", async () => {
    for (const [x, mode] of [[936, "Save"], [974, "Load"]]) {
      await page.click(x, 17);
      const u = await page.waitFor("the slot sheet", (v) => v.home.page === "Slots" && v.panel === "savestates" && v.menu_open, 4000);
      if (u.slots.mode !== mode) return `the ${mode} icon opened the sheet in ${u.slots.mode} mode`;
      if (u.running) return "the game still runs behind the sheet";
      await page.tapKey("Escape");
      await page.waitFor("the game running again", (v) => v.running && !v.menu_open && v.panel === "none", 5000);
    }
  });
  await step("toolbar: controller icon opens the controls panel", async () => {
    let s = await page.ux();
    if (s.panel !== "none") { await page.tapKey("Escape"); await sleep(300); }
    await page.click(1012, 17);
    // Controls is a section of the shell now: the game pauses and the shell opens on it.
    await page.waitFor("the Controls section", (x) => x.panel === "controls" && x.home.page === "Controls" && x.menu_open, 4000);
    await page.tapKey("Escape");
    await page.waitFor("Home", (x) => x.panel === "none" && x.home.page === "Home", 4000);
    await page.tapKey("Escape");
    await page.waitFor("resumed", (x) => x.running && !x.menu_open, 4000);
  });
  await step("toolbar: pause stops the guest and play resumes it", async () => {
    let s = await page.ux();
    if (!s.running || s.menu_open) await page.tapKey("Escape");
    await page.click(843, 17);
    await page.waitFor("paused", (x) => !x.running, 4000);
    await page.click(843, 17);
    await page.waitFor("running", (x) => x.running, 4000);
  });
  const settingsValue = async (row) => {
    await page.tapKey("Escape"); await page.waitFor("menu", (x) => x.menu_open, 4000);
    const value = await rowValue("Settings", row);
    await closeShell(page);
    return value;
  };
  await step("toolbar: the volume slider changes Settings / Volume", async () => {
    const vol0 = await settingsValue("Volume");
    await page.mouse("mouseMoved", 1055, 17); await page.mouse("mousePressed", 1055, 17); await sleep(60);
    await page.mouse("mouseMoved", 1100, 17); await sleep(60); await page.mouse("mouseReleased", 1100, 17); await sleep(200);
    const vol1 = await settingsValue("Volume");
    return vol0 === vol1 ? `volume stayed ${vol0}` : true;
  });
  await step("toolbar: the mute icon flips Settings / Mute, and again flips it back", async () => {
    const mute0 = await settingsValue("Mute");
    await page.click(1147, 17);
    const mute1 = await settingsValue("Mute");
    await page.click(1147, 17);
    const mute2 = await settingsValue("Mute");
    if (mute0 === mute1) return `mute stayed ${mute0}`;
    return mute2 === mute0 ? true : `a second click left mute at ${mute2}, not ${mute0}`;
  });
  await step("toolbar: debug and collapse buttons keep the game running", async () => {
    await page.click(1202, 17);
    await sleep(500);
    await page.shot(`${label}-13-debug`);
    await page.click(1202, 17);
    await page.click(1257, 17);
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



// ---------------------------------------------------------------- momentum scrolling of the shell's lists

/**
 * The shell's list pages scroll the way a phone's do: the content follows the finger, keeps coasting after it
 * lifts and slows smoothly, stretches against the ends and springs back, a finger put down on a coasting list
 * only stops it, and sliders and row taps are left alone. Desktop: the wheel scrolls it and ends a coast.
 */
async function journeyMomentum(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.goto(urlFor());
  const activate = async (name) => (mobile ? homeTap(page, name) : homeClick(page, name));
  /** [scroll, furthest scroll, view top, view bottom, 1 while it coasts or is held]. */
  const list = async () => (await page.ux()).home.list;
  const x = Math.min(40, size.w / 4);
  let flickSpeed = 0;
  /** Drag over the list from `from` to `to` (css px) in `steps` frames; lift at once, or after `rest` ms. */
  const drag = async (from, to, steps, rest = 0, lift = true) => {
    const t0 = Date.now();
    await page.touch("touchStart", [[x, from]]);
    for (let i = 1; i <= steps; i++) { await sleep(16); await page.touch("touchMove", [[x, from + ((to - from) * i) / steps]]); }
    flickSpeed = (Math.abs(to - from) / Math.max(1, Date.now() - t0)) * 1000;
    if (rest) await sleep(rest);
    if (lift) await page.touch("touchEnd", []);
  };
  const settled = () => page.waitFor("the list at rest", (s) => s.home.list[4] === 0, 8000);
  const toTop = async () => {
    for (let i = 0; i < 20 && (await list())[0] > 0.5; i++) { const v = await list(); await drag(v[2] + 12, v[3] - 12, 8, 140); await sleep(300); }
    await settled();
  };
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
  await page.tapKey("x");
  await step("Settings opens", async () => {
    await activate("nav:Settings");
    await page.waitFor("the Settings page", (s) => s.home.page === "Settings", 4000);
    await sleep(900);
    const v = await list();
    return v[1] > 100 ? true : `the list only scrolls ${v[1]} px`;
  });
  if (mobile) {
    await step("a flick keeps the list coasting after the finger lifts, slowing down, and it comes to rest inside the list", async () => {
      await toTop();
      const v0 = await list();
      const [top, bottom] = [v0[2], v0[3]];
      await drag(bottom - 12, bottom - 12 - Math.min(150, (bottom - top) * 0.55), 6);
      const lifted = (await list())[0];
      const trace = [{ t: Date.now(), p: lifted }];
      for (let i = 0; i < 10; i++) { await sleep(60); trace.push({ t: Date.now(), p: (await list())[0] }); }
      const samples = trace.map((x) => x.p);
      const max = v0[1];
      const reachedEnd = samples.some((v) => v >= max - 1);
      if (!reachedEnd && samples[samples.length - 1] < lifted + 25) return `after a ${flickSpeed.toFixed(0)} px/s flick it moved ${(samples[samples.length - 1] - lifted).toFixed(0)} px after the finger lifted: ${samples.map((v) => v.toFixed(0))}`;
      if (!reachedEnd) {
        const slow = coastProblem(trace);
        if (slow) return slow;
      }
      const end = await settled();
      await page.shot(`${label}-1-after-flick`);
      const e = end.home.list;
      if (e[0] < -0.5 || e[0] > e[1] + 0.5) return `it came to rest outside the list: ${e[0]} of ${e[1]}`;
      if (Math.max(...samples) > max + 0.35 * (bottom - top) + 2) return `it stretched past the end by ${Math.max(...samples) - max}`;
    });
    await step("a finger put down on the coasting list stops it where it is and presses no row", async () => {
      await toTop();
      const v = await list();
      const [top, bottom] = [v[2], v[3]];
      const row0 = (await page.ux()).menu.row;
      await drag(bottom - 12, top + 12, 6);
      await sleep(120);
      const moving = await list();
      if (moving[4] !== 1) return `it was not coasting (${moving})`;
      // Put the finger down on a row: the row that is under it must not be pressed.
      await page.tap(size.w / 2, (top + bottom) / 2, 60);
      const at = (await list())[0];
      await sleep(500);
      const later = await list();
      if (Math.abs(later[0] - at) > 1.5 || later[4] !== 0) return `it kept going after the touch: ${at} then ${later[0]} (moving ${later[4]})`;
      const u = await page.ux();
      if (u.menu.row !== row0) return `the touch that stopped it pressed a row: ${row0} to ${u.menu.row}`;
      // The next tap is a real tap again: it selects the row under it.
      await page.tap(size.w / 2, (top + bottom) / 2, 60);
      await sleep(300);
      if ((await page.ux()).menu.row === row0) return "after it stopped, a tap on a row did nothing";
    });
    await step("pulled past the top the list stretches under the finger, then springs back without crossing", async () => {
      await toTop();
      const v = await list();
      const [top, bottom] = [v[2], v[3]];
      await drag(top + 20, top + 20 + (bottom - top) * 0.7, 10, 140, false);
      const held = (await list())[0];
      await page.shot(`${label}-2-stretched-top`);
      await page.touch("touchEnd", []);
      if (held > -12) return `held ${((bottom - top) * 0.7).toFixed(0)} px down at the top, the position is ${held} (no stretch)`;
      if (held < -0.4 * (bottom - top)) return `stretched too far: ${held}`;
      const back = [];
      for (let i = 0; i < 14; i++) { await sleep(60); back.push((await list())[0]); }
      if (back.some((p) => p > 0.5)) return `crossed the top on the way back: ${back}`;
      const end = await list();
      if (end[0] !== 0 || end[4] !== 0) return `did not settle at the top: ${end}`;
    });
    await step("pulled past the bottom it stretches the other way and springs back", async () => {
      let v = await list();
      for (let i = 0; i < 12 && v[0] < v[1] - 1; i++) { await drag(v[3] - 12, v[2] + 12, 8, 140); await sleep(300); v = await list(); }
      await settled();
      v = await list();
      if (v[0] < v[1] - 1) return `could not reach the bottom: ${v[0]} of ${v[1]}`;
      await drag(v[3] - 20, v[3] - 20 - (v[3] - v[2]) * 0.7, 10, 140, false);
      const held = await list();
      await page.shot(`${label}-3-stretched-bottom`);
      await page.touch("touchEnd", []);
      if (held[0] < held[1] + 12) return `held at the bottom, the position is ${held[0]} of ${held[1]} (no stretch)`;
      await sleep(900);
      const end = await list();
      if (Math.abs(end[0] - end[1]) > 0.6 || end[4] !== 0) return `did not settle at the bottom: ${end}`;
    });
    await step("a drag that starts on a slider moves the slider, not the list", async () => {
      await toTop();
      const u = await page.ux();
      const slider = u.home.hits.find((h) => h[0].startsWith("slider:"));
      if (!slider) return true;
      const before = (await list())[0];
      const cy = (slider[2] + slider[4]) / 2;
      await page.touch("touchStart", [[slider[1] + (slider[3] - slider[1]) * 0.3, cy]]);
      for (let i = 1; i <= 8; i++) { await sleep(16); await page.touch("touchMove", [[slider[1] + (slider[3] - slider[1]) * (0.3 + 0.04 * i), cy - 12 * i]]); }
      await page.touch("touchEnd", []);
      await sleep(400);
      const after = await list();
      return Math.abs(after[0] - before) < 1 ? true : `the list moved ${before} to ${after[0]} under a slider drag`;
    });
  } else {
    await step("the wheel scrolls the list and ends at the top and bottom without stretching", async () => {
      const v = await list();
      const [mx, my] = [size.w / 2, (v[2] + v[3]) / 2];
      await page.mouse("mouseMoved", mx, my);
      for (let i = 0; i < 4; i++) await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: mx, y: my, deltaX: 0, deltaY: 100 }, page.top);
      await sleep(500);
      const after = await list();
      if (!(after[0] > 100)) return `four wheel notches moved the list to ${after[0]}`;
      for (let i = 0; i < 80; i++) await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: mx, y: my, deltaX: 0, deltaY: 300 }, page.top);
      await sleep(700);
      const end = await list();
      if (Math.abs(end[0] - end[1]) > 0.6) return `a long wheel run ended at ${end[0]} of ${end[1]}`;
      for (let i = 0; i < 80; i++) await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: mx, y: my, deltaX: 0, deltaY: -300 }, page.top);
      await sleep(700);
      const top = await list();
      if (top[0] !== 0) return `scrolling back up ended at ${top[0]}`;
    });
  }
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

// ---------------------------------------------------------------- the home screen

/**
 * The list pages (Settings, Game): the view ends above the phone's tab bar, the last row can scroll fully
 * clear of it, and the up and down hints show only while rows are hidden past that end. Driven by a touch
 * drag (phones), the wheel (desktop) and the keyboard.
 */
async function journeyList(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.goto(urlFor());
  const activate = async (name) => (mobile ? homeTap(page, name) : homeClick(page, name));
  /** What the list shows now: its scroll, the furthest scroll, the view, the tab bar's top and the rows drawn. */
  const look = async () => {
    const h = (await page.ux()).home;
    const [scroll, max, top, bottom] = h.list;
    const rows = h.hits.filter((x) => x[0].startsWith("row:")).map((x) => ({ name: x[0].slice(4), top: x[2], bottom: x[4] }));
    const bar = h.hits.filter((x) => x[0].startsWith("nav:")).map((x) => x[2]);
    return { scroll, max, top, bottom, rows, barTop: mobile && bar.length ? Math.min(...bar) : size.h };
  };
  const settle = async () => { await sleep(350); };
  const whole = (r, v) => r.top >= v.top - 1 && r.bottom <= v.bottom + 1;
  const drag = async (dy) => {
    const v = await look();
    const x = 32, y0 = dy < 0 ? v.bottom - 10 : v.top + 10;
    await page.touch("touchStart", [[x, y0]]);
    for (let i = 1; i <= 8; i++) { await sleep(16); await page.touch("touchMove", [[x, y0 + (dy * i) / 8]]); }
    // The finger rests before it lifts: a placed drag, not a flick.
    await sleep(140);
    await page.touch("touchEnd", []);
    await settle();
  };
  const wheel = async (dy) => {
    const v = await look();
    await page.mouse("mouseMoved", size.w / 2, (v.top + v.bottom) / 2);
    await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: size.w / 2, y: (v.top + v.bottom) / 2, deltaX: 0, deltaY: dy }, page.top);
    await settle();
  };
  const scrollBy = (dy) => (mobile ? drag(-dy) : wheel(dy));
  /** The checks that hold wherever the list is scrolled to. */
  const sane = (v, where) => {
    if (v.bottom > v.barTop - 4) return `${where}: the view ends at ${v.bottom}, under the tab bar at ${v.barTop}`;
    if (v.max < 1) return `${where}: the list fits (max ${v.max}), so there is nothing to scroll`;
    return null;
  };
  const pageSteps = async (column) => {
    await step(`${column}: at the top the first row is whole and only the down hint applies (${label})`, async () => {
      await settle();
      let v = await look();
      for (let i = 0; i < 40 && v.scroll > 1; i++) { await scrollBy(-400); v = await look(); }
      await page.shot(`${label}-${column.toLowerCase()}-1-top`);
      const bad = sane(v, "top");
      if (bad) return bad;
      if (v.scroll > 1) return `could not reach the top: ${v.scroll}`;
      const first = v.rows.reduce((a, b) => (a.top < b.top ? a : b));
      if (!whole(first, v)) return `the first row ${first.name} is cut at the top: ${first.top} vs ${v.top}`;
    });
    await step(`${column}: scrolled to the middle rows are hidden both ways`, async () => {
      await scrollBy(Math.max(60, (await look()).max / 2));
      const v = await look();
      await page.shot(`${label}-${column.toLowerCase()}-2-middle`);
      if (!(v.scroll > 1 && v.scroll < v.max - 1)) return `not in the middle: ${v.scroll} of ${v.max}`;
    });
    await step(`${column}: at the bottom the last row is whole and clear of the tab bar, the down hint is gone`, async () => {
      let v = await look();
      for (let i = 0; i < 40 && v.scroll < v.max - 1; i++) { await scrollBy(400); v = await look(); }
      await page.shot(`${label}-${column.toLowerCase()}-3-bottom`);
      const bad = sane(v, "bottom");
      if (bad) return bad;
      if (v.scroll < v.max - 1) return `could not reach the bottom: ${v.scroll} of ${v.max}`;
      const last = v.rows.reduce((a, b) => (a.bottom > b.bottom ? a : b));
      if (!whole(last, v)) return `the last row ${last.name} is cut: ${last.bottom} vs the view's ${v.bottom}`;
      if (last.bottom > v.barTop - 4) return `the last row ${last.name} ends at ${last.bottom}, under the tab bar at ${v.barTop}`;
    });
    await step(`${column}: the keyboard walks to the last row and back, the scroll follows the focus`, async () => {
      for (let i = 0; i < 60; i++) await page.tapKey("ArrowDown");
      await settle();
      let v = await look();
      await page.shot(`${label}-${column.toLowerCase()}-4-key-bottom`);
      const last = v.rows.reduce((a, b) => (a.bottom > b.bottom ? a : b));
      if (!whole(last, v) || last.bottom > v.barTop - 4) return `the focused last row is not clear: ${JSON.stringify(last)} view ${v.bottom} bar ${v.barTop}`;
      if (v.scroll < v.max - 1) return `the bottom row is focused but the list is at ${v.scroll} of ${v.max}`;
      for (let i = 0; i < 60; i++) await page.tapKey("ArrowUp");
      await settle();
      v = await look();
      await page.shot(`${label}-${column.toLowerCase()}-5-key-top`);
      if (v.scroll > 1) return `back at the first row the list is at ${v.scroll}`;
    });
  };
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
  await page.tapKey("x");
  await step("Settings opens and has no Controls row", async () => {
    await activate("nav:Settings");
    await page.waitFor("the Settings page", (s) => s.home.page === "Settings", 4000);
    await sleep(900);
    const v = await look();
    if (v.rows.some((r) => r.name === "Controls")) return "Settings lists Controls";
  });
  await pageSteps("Settings");
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** A point on the home screen that no hit area covers (pads of 14 px), inside the safe area. */
async function emptyPoint(page, size) {
  const u = await page.ux();
  const [l, t, r, b] = insetsFor(size);
  for (let fy = 0.22; fy < 0.95; fy += 0.03) {
    for (let fx = 0.1; fx < 0.95; fx += 0.04) {
      const x = fx * size.w, y = fy * size.h;
      if (x < l + 10 || x > size.w - r - 10 || y < t + 10 || y > size.h - b - 10) continue;
      if (!u.home.hits.some(([, x0, y0, x1, y1]) => x > x0 - 14 && x < x1 + 14 && y > y0 - 14 && y < y1 + 14)) return [x, y];
    }
  }
  throw new Error("no empty point on the home screen");
}

/** Safe-area insets the emulation applies (see Page.applyEmulation), as [left, top, right, bottom]. */
function insetsFor(size) {
  if (!size.mobile) return [0, 0, 0, 0];
  return size.w < size.h ? [0, 47, 0, 34] : [47, 0, 47, 21];
}

/** Named hit areas (not the cases, which tilt off the edges on purpose) lie inside the safe area and apart. */
function homeLayoutProblem(u, size) {
  const [l, t, r, b] = insetsFor(size);
  const named = u.home.hits.filter(([n]) => /^(nav|primary|peek|welcome|link|back|logo|row|welcome-step|dismiss|close|restart|ctl:tab)/.test(n) && !/^row/.test(n));
  for (const [n, x0, y0, x1, y1] of named) {
    if (x0 < l - 1 || y0 < t - 1 || x1 > size.w - r + 1 || y1 > size.h - b + 1) return `${n} [${x0},${y0},${x1},${y1}] is outside the safe area [${l},${t},${size.w - r},${size.h - b}]`;
  }
  const shrink = (h) => [h[1] + 8, h[2] + 8, h[3] - 8, h[4] - 8]; // hit areas are padded; compare the drawn core
  for (let i = 0; i < named.length; i++) {
    for (let j = i + 1; j < named.length; j++) {
      const [a0, b0, a1, b1] = shrink(named[i]), [c0, d0, c1, d1] = shrink(named[j]);
      if (Math.min(a1, c1) - Math.max(a0, c0) > 1 && Math.min(b1, d1) - Math.max(b0, d0) > 1) return `${named[i][0]} and ${named[j][0]} overlap`;
    }
  }
  return null;
}

const ONE_PIXEL_PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/** Answer requests to the cover collections: "hit", "miss" (404) or "offline", and log them. */
async function stubCovers(page, mode) {
  const state = { mode, urls: [], held: [] };
  await page.cdp.send("Fetch.enable", { patterns: [{ urlPattern: "https://raw.githubusercontent.com/*" }] }, page.top);
  page.cdp.on((msg) => {
    if (msg.method !== "Fetch.requestPaused" || msg.sessionId !== page.top) return;
    const id = msg.params.requestId;
    state.urls.push(msg.params.request.url);
    const cors = [{ name: "Access-Control-Allow-Origin", value: "*" }];
    // A page that is already closed answers nothing: never let that reject unhandled.
    const quiet = (promise) => promise.catch(() => {});
    if (state.mode === "hold") { state.held.push(id); return; }
    if (state.mode === "offline") quiet(page.cdp.send("Fetch.failRequest", { requestId: id, errorReason: "InternetDisconnected" }, page.top));
    else if (state.mode === "hit") quiet(page.cdp.send("Fetch.fulfillRequest", { requestId: id, responseCode: 200, responseHeaders: [...cors, { name: "Content-Type", value: "image/png" }], body: ONE_PIXEL_PNG }, page.top));
    else quiet(page.cdp.send("Fetch.fulfillRequest", { requestId: id, responseCode: 404, responseHeaders: cors, body: "" }, page.top));
  });
  /** Answer every request held so far: "hit" or "miss", and answer later ones the same way. */
  state.release = (kind) => {
    const held = state.held.splice(0);
    state.mode = kind;
    const cors = [{ name: "Access-Control-Allow-Origin", value: "*" }];
    for (const id of held) {
      if (kind === "hit") page.cdp.send("Fetch.fulfillRequest", { requestId: id, responseCode: 200, responseHeaders: [...cors, { name: "Content-Type", value: "image/png" }], body: ONE_PIXEL_PNG }, page.top).catch(() => {});
      else page.cdp.send("Fetch.fulfillRequest", { requestId: id, responseCode: 404, responseHeaders: cors, body: "" }, page.top).catch(() => {});
    }
  };
  return state;
}

const FAKE_FOLDER = `
  class FakeDir { constructor() { this.kind = "directory"; this.name = "games"; }
    async *entries() { for (const n of ["Crash Bandicoot (USA).bin", "Nothing Here (USA).bin", "notes.txt"]) yield [n, { kind: "file", name: n }]; } }
  window.showDirectoryPicker = async () => new FakeDir();
`;

/** One visitor, from the first countdown to the settings controls. */
async function journeyHome(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const open = async (init) => {
    const page = await newPage(label);
    await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
    // The iPhone silent-switch note has its own journey; here it only covers the page.
    await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
    if (init) await page.addInit(init);
    await page.goto(urlFor());
    return page;
  };
  const close = (page) => cdp.send("Target.closeTarget", { targetId: page.targetId });
  const activate = async (page, name) => (mobile ? homeTap(page, name) : homeClick(page, name));
  const countdown = (page) => page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);

  let page = await open();
  await step("first visit: Home opens at once on the demo disc, with the countdown inline and nothing on top", async () => {
    const u = await countdown(page);
    await page.shot(`${label}-01-first-visit`);
    const names = u.home.hits.map((h) => h[0]);
    for (const need of ["case:0", "primary", "nav:Home", "nav:Controls", "nav:Settings", "peek", "welcome"]) if (!names.includes(need)) return `missing ${need}: ${names}`;
    if (u.home.page !== "Home" || u.running || !u.menu_open) return JSON.stringify(u.home);
    return homeLayoutProblem(u, size) ?? true;
  });
  if (mobile) await step("the home screen shows the shell header only: no touch controller on it", async () => {
    const u = await page.ux();
    return u.touch ? `the touch controller is drawn on the home screen: ${JSON.stringify(u.touch.ui)}` : true;
  });
  await step("'What is on it' opens on demand and closes again", async () => {
    await activate(page, "peek");
    const on = await page.waitFor("the peek", (s) => s.home.peek, 4000);
    await page.shot(`${label}-02-peek`);
    await activate(page, "peek");
    await page.waitFor("peek closed", (s) => !s.home.peek, 4000);
    return on.home.peek ? true : "no peek";
  });
  await close(page);

  // Every kind of input cancels the countdown, and the visitor stays on Home.
  const inputs = [
    ["a key", async (p) => p.tapKey("x")],
    ["a click", async (p) => { const [x, y] = await emptyPoint(p, size); await p.click(x, y); }],
    ["a tap", async (p) => { const [x, y] = await emptyPoint(p, size); await p.tap(x, y); }],
    ["a pad button", async (p) => { await p.padConnect(); await sleep(300); await p.padSet({ 0: true }); await sleep(200); await p.padSet({}); }],
    ["a stick", async (p) => { await p.padConnect(); await sleep(300); await p.padSet({}, [0.9, 0, 0, 0]); await sleep(300); await p.padSet({}, [0, 0, 0, 0]); }],
  ].filter(([name]) => (mobile ? name !== "a click" : name !== "a tap"));
  for (const [name, send] of inputs) {
    page = await open();
    await step(`the countdown is cancelled by ${name} and the visitor stays on Home`, async () => {
      await countdown(page);
      await send(page);
      await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 5000);
      await sleep(4500);
      const u = await page.ux();
      if (u.running || !u.menu_open || u.home.page !== "Home") return JSON.stringify({ running: u.running, open: u.menu_open, home: u.home.page });
      await page.shot(`${label}-03-cancelled-${name.replace(/\W+/g, "-")}`);
      return true;
    });
    if (name === "a key" || name === "a tap") {
      await step("a return visit stays on Home with no countdown, and Start disc starts the disc", async () => {
        await page.goto(urlFor());
        await page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
        await sleep(5000);
        const u = await page.ux();
        if (u.running || u.home.countdown !== "off") return `not staying home: ${JSON.stringify(u.home)}`;
        await page.shot(`${label}-04-return-visit`);
        await activate(page, "primary");
        await waitDemoRunning(page);
      });
    }
    await close(page);
  }

  // Escape, Select+Start and the touch Menu button are Back on the home screen: they never start the disc.
  page = await open();
  await step("Escape, the pad chord and the touch menu button go back on Home and never start the disc", async () => {
    await countdown(page);
    await page.tapKey("Escape");
    await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 5000);
    await sleep(1500);
    let u = await page.ux();
    if (u.running || !u.menu_open) return `Escape started the disc: ${JSON.stringify({ running: u.running, open: u.menu_open })}`;
    await activate(page, "nav:Settings");
    await page.waitFor("the Settings page", (s) => s.home.page === "Settings", 4000);
    await page.tapKey("Escape");
    await page.waitFor("Home again", (s) => s.home.page === "Home", 4000);
    u = await page.ux();
    if (u.running || !u.menu_open) return `Escape on a page left Home: ${JSON.stringify({ running: u.running, open: u.menu_open })}`;
    if (mobile) {
      // The controller belongs to a game: on the home screen there is no menu button to press (Back is
      // Escape or the shell's own bar), and the key goes back like everywhere else.
      if (u.touch) return `the touch controller is drawn on the home screen: ${JSON.stringify(u.touch.ui)}`;
      await page.tapKey("Escape");
    } else {
      await page.padConnect();
      await sleep(300);
      await page.padSet({ 8: true, 9: true });
      await sleep(300);
      await page.padSet({});
    }
    await sleep(1500);
    u = await page.ux();
    return u.running || !u.menu_open ? `the ${mobile ? "touch Back key" : "pad chord"} started the disc` : true;
  });
  await step("Back (Circle) never starts the disc either, and Enter still does", async () => {
    await page.tapKey("g");
    await sleep(800);
    if ((await page.ux()).running) return "G (Circle) started the disc";
    await page.tapKey("Enter");
    await waitDemoRunning(page, 30000);
  });
  await close(page);

  // Navigation and focus.
  page = await open();
  await step("Controls and Settings: each opens inside the shell, fits the screen, and Back returns Home; there is no Debug tab", async () => {
    await countdown(page);
    if ((await page.ux()).home.hits.some((h) => h[0] === "nav:Debug")) return "Debug is still a tab";
    for (const section of ["Controls", "Settings"]) {
      await activate(page, `nav:${section}`);
      await page.waitFor(`the ${section} page`, (s) => s.home.page === section, 4000);
      await sleep(600);
      await page.shot(`${label}-05-${section.toLowerCase()}`);
      const u = await page.ux();
      const problem = homeLayoutProblem(u, size);
      if (section === "Controls" && u.panel !== "controls") return `Controls is not a section of the shell: panel=${u.panel}`;
      if (problem) return `${section}: ${problem}`;
      await page.tapKey("g");
      await page.waitFor("Home again", (s) => s.home.page === "Home", 4000);
    }
    return true;
  });
  await step("the keyboard moves focus: right changes the case, up reaches the nav, Enter opens it", async () => {
    await page.tapKey("ArrowRight");
    const moved = await page.waitFor("the second case", (s) => s.home.case === 1, 4000);
    await page.tapKey("ArrowLeft");
    await page.tapKey("ArrowUp");
    await page.waitFor("the nav row", (s) => s.home.zone === "nav", 4000);
    await page.tapKey("ArrowRight");
    await page.tapKey("Enter");
    await page.waitFor("the Settings page", (s) => s.home.page === "Settings", 4000);
    await page.tapKey("g");
    await page.waitFor("Home again", (s) => s.home.page === "Home", 4000);
    return moved.home.case === 1 ? true : "focus did not move";
  });
  await step("L1/R1 (Q/E, PageUp/PageDown) cycle the sections from anywhere and wrap", async () => {
    const names = ["Home", "Settings", "Controls"];
    const at = async (want) => { await page.waitFor(`the ${want} page`, (s) => s.home.page === want, 4000); };
    for (let i = 1; i <= 3; i++) { await page.tapKey("PageDown"); await at(names[i % 3]); }
    await page.tapKey("PageUp"); await at("Controls");
    await page.tapKey("PageUp"); await at("Settings");
    await page.tapKey("e"); await at("Controls");
    await page.tapKey("q"); await at("Settings");
    if (!mobile) {
      await page.padConnect();
      await sleep(300);
      const tap = async (b) => { await page.padSet({ [b]: true }); await sleep(200); await page.padSet({}); await sleep(250); };
      await tap(5); await at("Controls");
      await tap(5); await at("Home");
      await tap(4); await at("Controls");
      await tap(4); await at("Settings");
    }
    await page.tapKey("g");
    await page.waitFor("Home again", (s) => s.home.page === "Home", 4000);
  });
  await step("every section is left the same visible ways: Back, Escape, the nav, the logo and a swipe or tap", async () => {
    for (const section of ["Controls", "Settings"]) {
      const back = async (how) => {
        await activate(page, `nav:${section}`);
        await page.waitFor(`the ${section} page`, (s) => s.home.page === section, 4000);
        await how();
        await page.waitFor(`Home after leaving ${section}`, (s) => s.home.page === "Home", 4000);
      };
      await back(async () => page.tapKey("g"));
      await back(async () => page.tapKey("Escape"));
      await back(async () => activate(page, "nav:Home"));
      await back(async () => activate(page, "logo"));
      if (!mobile) {
        await back(async () => { await page.padConnect(); await sleep(200); await page.padSet({ 1: true }); await sleep(200); await page.padSet({}); });
      } else {
        await back(async () => homeTap(page, "nav:Home"));
      }
    }
  });
  await step("the welcome pages page forward and back, and Close returns Home", async () => {
    await activate(page, "welcome");
    await page.waitFor("the welcome pages", (s) => s.home.page === "Welcome", 4000);
    await page.shot(`${label}-07-welcome`);
    const u = await page.ux();
    const problem = homeLayoutProblem(u, size);
    await activate(page, "welcome-step:1");
    await activate(page, "welcome-step:-1");
    await activate(page, "dismiss");
    await page.waitFor("Home again", (s) => s.home.page === "Home", 4000);
    return problem ?? true;
  });
  await step("Settings rows are real controls: a toggle flips and a slider drags", async () => {
    await activate(page, "nav:Settings");
    await page.waitFor("the Settings page", (s) => s.home.page === "Settings", 4000);
    const before = await page.ux();
    const row = (u, name) => u.home.hits.find((h) => h[0] === name);
    await menuSelect(page, "Settings", "Mute", { enter: false });
    const mute0 = (await page.ux()).menu.value;
    await page.tapKey("ArrowRight");
    await sleep(300);
    const mute1 = (await page.ux()).menu.value;
    if (mute0 === mute1) return `Mute stayed ${mute0}`;
    await page.tapKey("ArrowRight"); // and back
    const vol0 = await rowValueOf(page, "Volume");
    const volume = row(await page.ux(), "slider:Volume");
    if (!volume) return `no volume slider: ${before.home.hits.map((h) => h[0])}`;
    const [x0, y0, x1, y1] = volume.slice(1);
    const y = (y0 + y1) / 2;
    await page.mouse("mouseMoved", x0 + 20, y);
    await page.mouse("mousePressed", x0 + 20, y);
    for (let i = 1; i <= 6; i++) { await sleep(100); await page.mouse("mouseMoved", x0 + 20 + i * ((x1 - x0) * 0.6 / 6), y); }
    await page.mouse("mouseReleased", x0 + 20 + (x1 - x0) * 0.6, y);
    await sleep(300);
    await page.shot(`${label}-08-settings-after`);
    const vol1 = await rowValueOf(page, "Volume");
    return vol0 !== vol1 ? true : `volume stayed ${vol0} after dragging the slider ${JSON.stringify(volume)} y=${y} now ${JSON.stringify((await page.ux()).home.hits.find((h) => h[0] === 'slider:Volume'))}`;
  });
  await close(page);
}

/** The value of a Settings row, read through the keyboard selection probe. */
async function rowValueOf(page, row) {
  await menuSelect(page, "Settings", row, { enter: false });
  return (await page.ux()).menu.value;
}

/** Box art: a hit, a miss and the network being offline, with the toggle. Needs the folder picker (top page). */
async function journeyCovers(label, urlFor) {
  current = label;
  const SEED = `try { localStorage.setItem("psoxide.recents", "web:Crash Bandicoot (USA).bin"); } catch (e) {}`;
  const pickFolder = async (page) => {
    await page.waitFor("the home screen", (s) => s.home?.page === "Home" && s.has_bus, 90000);
    await page.tapKey("x"); // cancel the countdown
    await homeClick(page, "case:1"); // focus the folder case
    await homeClick(page, "primary"); // Choose folder -> the fake picker
    await page.waitFor("the folder scanned", (s) => /Found 2/.test(s.status) || s.home.hits.some((h) => h[0] === "row:x"), 15000).catch(() => {});
    await sleep(800);
  };
  const library = async (page) => {
    // The shelf is Home: its cases ask for their pictures as they come into view.
    await sleep(2500);
  };
  for (const mode of ["hit", "miss", "offline", "switched-off"]) {
    const page = await newPage(label);
    await page.viewport(1280, 800, { dpr: 1 });
    const net = await stubCovers(page, mode === "switched-off" ? "hit" : mode);
    await page.addInit(FAKE_FOLDER);
    await page.addInit(SEED);
    await page.goto(urlFor());
    if (mode === "switched-off") {
      await step("box art switched off in Settings: opening a folder fetches nothing", async () => {
        await page.waitFor("the home screen", (s) => s.home?.page === "Home" && s.has_bus, 90000);
        await page.tapKey("x");
        await menuSelect(page, "Settings", "Box art from the internet", { enter: false });
        await page.tapKey("ArrowRight");
        await page.waitFor("the row to read Off", (s) => s.menu.value === "Off", 4000);
        await page.tapKey("g");
        await homeClick(page, "case:1");
        await homeClick(page, "primary");
        await sleep(1000);
        await library(page);
        return net.urls.length === 0 ? true : `fetched while switched off: ${net.urls}`;
      });
      await cdp.send("Target.closeTarget", { targetId: page.targetId });
      continue;
    }
    await step(`cover fetch (${mode}): the shelf asks the collections by file name`, async () => {
      await pickFolder(page);
      await library(page);
      const u = await page.ux();
      await page.shot(`${label}-cover-${mode}`);
      const asked = net.urls.some((x) => /Named_Boxarts\/Crash%20Bandicoot%20%28USA%29\.png$/.test(x));
      if (!asked) return `the collection was not asked: ${JSON.stringify(net.urls)}`;
      if (mode === "hit" && !(u.home.covers >= 1)) return `no picture was kept: ${u.home.covers}`;
      if (mode !== "hit" && u.home.covers !== 0) return `a picture appeared on a ${mode}: ${u.home.covers}`;
      return true;
    });
    if (mode === "hit") {
      await step("only discs this browser really played are labelled as recent", async () => {
        const u = await page.ux();
        const lines = Object.fromEntries(u.home.cases.map(([t, l]) => [t, l]));
        if (lines["Crash Bandicoot (USA)"] !== "Played recently") return `the played disc is not a recent: ${JSON.stringify(lines)}`;
        if (lines["Nothing Here (USA)"] === "Played recently") return "an unplayed disc is labelled as recent";
        await page.shot(`${label}-recents`);
      });
      await step("a cached cover is not fetched again after a reload, and a saved folder can be reopened", async () => {
        const before = net.urls.length;
        await page.goto(urlFor());
        await page.waitFor("the home screen", (s) => s.home?.page === "Home" && s.has_bus, 90000);
        await sleep(2500);
        if (net.urls.length !== before) return `refetched after reload: ${net.urls.slice(before)}`;
        const u = await page.ux();
        if (u.home.cases.some(([, l]) => l === "Played recently")) return "recents survived without their folder";
        const cases = (await page.ux()).home.cases.map(([t]) => t);
        return cases.includes("Reopen your games folder") ? true : `no Reopen case: ${cases}`;
      });
    }
    await cdp.send("Target.closeTarget", { targetId: page.targetId });
  }
  const page = await newPage(label);
  await page.viewport(1280, 800, { dpr: 1 });
  await stubCovers(page, "miss");
  await page.addInit(FAKE_FOLDER);
  await page.goto(urlFor());
  await step("with no play history, a folder's discs are not offered as recents", async () => {
    await pickFolder(page);
    await page.tapKey("g");
    const lines = (await page.ux()).home.cases.map(([, l]) => l);
    return lines.includes("Played recently") ? `a recent appeared with no history: ${lines}` : true;
  });
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}



/** A folder of rips as they come off a drive: tracks beside a cue sheet, and a game over two discs. */
const FAKE_RIPS = `
  const track = (stem, n, pad) => stem + " (Track " + String(n).padStart(pad, "0") + ").bin";
  const cue = (files) => files.map((f, i) => 'FILE "' + f + '" BINARY\\n  TRACK ' + String(i + 1).padStart(2, "0") + ' MODE2/2352\\n    INDEX 01 00:00:00').join("\\n");
  const rip = (stem, count, pad) => {
    const bins = [...Array(count)].map((_, i) => track(stem, i + 1, pad));
    return { ...Object.fromEntries(bins.map((b) => [b, ""])), [stem + ".cue"]: cue(bins) };
  };
  const disc = (stem) => ({ [stem + ".bin"]: "", [stem + ".cue"]: cue([stem + ".bin"]) });
  const tree = {
    "WipEout (USA)": rip("WipEout (USA)", 9, 1),
    "WipEout 2097 (Europe)": rip("WipEout 2097 (Europe)", 12, 2),
    "Final Fantasy IX (USA) (Disc 1)": disc("Final Fantasy IX (USA) (Disc 1)"),
    "Final Fantasy IX (USA) (Disc 2)": disc("Final Fantasy IX (USA) (Disc 2)"),
    "Crash Bandicoot (USA).bin": "",
    "notes.txt": "",
  };
  class Node {
    constructor(name, v) { this.name = name; this.v = v; this.kind = typeof v === "string" ? "file" : "directory"; }
    async *entries() { for (const [n, v] of Object.entries(this.v)) yield [n, new Node(n, v)]; }
    async getFile() { const t = this.v; return { size: t.length, slice: () => ({ text: async () => t }) }; }
  }
  window.showDirectoryPicker = async () => new Node("games", tree);
`;

/** A games folder shows games, never tracks: one case per game, discs of one game together. */
async function journeyGrouping(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await stubCovers(page, "miss");
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.addInit(FAKE_RIPS);
  await page.goto(urlFor());
  const activate = async (name) => (mobile ? homeTap(page, name) : homeClick(page, name));
  await step("a folder of rips lists one case per game: no tracks, discs together", async () => {
    await page.waitFor("the home screen", (s) => s.home?.page === "Home" && s.has_bus, 90000);
    await page.tapKey("x");
    await activate("case:1");
    await activate("primary");
    await page.waitFor("the folder scanned", (s) => /Found \d+/.test(s.status), 15000);
    await sleep(600);
    const u = await page.ux();
    const titles = u.home.cases.map(([t]) => t);
    await page.shot(`${label}-grouped-shelf`);
    const want = ["Crash Bandicoot (USA)", "Final Fantasy IX (USA)", "WipEout (USA)", "WipEout 2097 (Europe)"];
    for (const w of want) if (!titles.includes(w)) return `missing the case "${w}": ${titles}`;
    const bad = titles.filter((t) => /\(Track|\(Disc \d\)/.test(t));
    if (bad.length) return `a track or disc is listed on its own: ${bad}`;
    if (!/^Found 4 /.test(u.status)) return `expected 4 games, status says: ${u.status}`;
    const ff = u.home.cases.find(([t]) => t === "Final Fantasy IX (USA)");
    return /2 discs/.test(ff[1]) ? true : `the two-disc game does not say so: ${ff[1]}`;
  });
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** While a picture is being looked up the case says so; the picture fades in; a miss settles quietly. */
async function journeyCoverStates(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  for (const outcome of ["hit", "miss"]) {
    const page = await newPage(label);
    await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
    const net = await stubCovers(page, "hold");
    await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
    await page.addInit(FAKE_FOLDER);
    await page.goto(urlFor());
    const activate = async (name) => (mobile ? homeTap(page, name) : homeClick(page, name));
    const crash = (u) => u.home.cases.find(([t]) => t === "Crash Bandicoot (USA)");
    await step(`a cover being fetched shows a loading state on its case, then (${outcome}) settles`, async () => {
      await page.waitFor("the home screen", (s) => s.home?.page === "Home" && s.has_bus, 90000);
      await page.tapKey("x");
      await activate("case:1");
      await activate("primary");
      await page.waitFor("the folder scanned", (s) => /Found \d+/.test(s.status), 15000);
      // Walk the focus onto the disc so its case is drawn big.
      for (let i = 0; i < 6; i++) {
        const u = await page.ux();
        if (u.home.cases[u.home.case]?.[0] === "Crash Bandicoot (USA)") break;
        await page.tapKey("ArrowRight");
        await sleep(150);
      }
      const loading = await page.waitFor("the case to show it is loading", (s) => {
        const c = crash(s);
        return c && c[2] === "loading" && c[4] > 0.5;
      }, 8000);
      await page.shot(`${label}-${outcome}-1-loading`);
      // Navigation is never blocked by the lookup.
      await page.tapKey("ArrowLeft");
      await page.waitFor("the focus to move while loading", (s) => s.home.case !== loading.home.case, 3000);
      await page.tapKey("ArrowRight");
      await sleep(300);
      net.release(outcome);
      if (outcome === "hit") {
        const u = await page.waitFor("the picture to fade in", (s) => { const c = crash(s); return c && c[2] === "ready" && c[5] > 0.95; }, 8000);
        await page.shot(`${label}-hit-2-ready`);
        return crash(u)[4] < 0.2 ? true : `the shimmer stayed on after the picture arrived: ${crash(u)}`;
      }
      const u = await page.waitFor("the lookup to settle on the placeholder", (s) => { const c = crash(s); return c && c[2] === "missing" && c[4] < 0.05; }, 8000);
      await page.shot(`${label}-miss-2-settled`);
      return crash(u)[5] === 0 ? true : `a picture is showing on a miss: ${crash(u)}`;
    });
    await cdp.send("Target.closeTarget", { targetId: page.targetId });
  }
}

/** The focused case comes alive and the others settle back; reduced motion keeps only a scale and a glow. */
async function journeySelection(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.goto(urlFor());
  await step("moving the focus lifts the new case and settles the old one", async () => {
    await page.waitFor("the home screen", (s) => s.home?.page === "Home" && s.has_bus, 90000);
    await page.tapKey("x");
    await sleep(900);
    const before = await page.ux();
    if (before.home.cases[0][3] < 0.9) return `the focused case is not at rest focused: ${before.home.cases[0]}`;
    await page.tapKey("ArrowRight");
    // Mid-move: the new case is rising, the old one is falling.
    await sleep(120);
    const mid = await page.ux();
    await page.shot(`${label}-focus-mid`);
    await sleep(900);
    const after = await page.ux();
    await page.shot(`${label}-focus-after`);
    if (!(mid.home.cases[1][3] > 0.05 && mid.home.cases[1][3] < 1.15)) return `the new case is not easing in: ${mid.home.cases[1]}`;
    if (after.home.cases[1][3] < 0.9 || after.home.cases[1][3] > 1.15) return `the new case did not settle focused: ${after.home.cases[1]}`;
    if (after.home.cases[0][3] > 0.1) return `the old case did not settle back: ${after.home.cases[0]}`;
    return true;
  });
  await step("Reduce motion in Settings is a switch and the shell reports it", async () => {
    await menuSelect(page, "Settings", "Reduce motion", { enter: false });
    await page.tapKey("ArrowRight");
    await page.waitFor("the row to read On", (s) => s.menu.value === "On", 4000);
    const u = await page.ux();
    return u.home.calm === true ? true : `the shell is not calm: ${u.home.calm}`;
  });
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** Button hints: the view switch and the tab bar's L1/R1 answer the pad, and show for what is in use. */
async function journeyHints(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.goto(urlFor());
  const names = async () => (await page.ux()).home.hits.map((h) => h[0]);
  await page.waitFor("the home screen", (s) => s.home?.page === "Home" && s.has_bus, 90000);
  await page.tapKey("x");
  await sleep(500);
  if (!mobile) await step("on a desktop the tab bar has L1 and R1 key caps for a keyboard, and the view switch is reachable", async () => {
    const n = await names();
    await page.shot(`${label}-hints-keyboard`);
    for (const need of ["tab-step:-1", "tab-step:1", "size"]) if (!n.includes(need)) return `missing ${need}: ${n}`;
    return true;
  });
  if (mobile) await step("a touch phone shows no pad hints until a controller is connected", async () => {
    const n = await names();
    return n.includes("tab-step:-1") ? "pad hints on a touch phone with no controller" : true;
  });
  await page.padConnect();
  await sleep(500);
  await page.padSet({ 4: false });
  await step("with a controller the tab bar ends carry L1 and R1, and Triangle switches cases and grid", async () => {
    await page.padSet({ 2: false });
    await sleep(200);
    const n = await names();
    await page.shot(`${label}-hints-pad`);
    for (const need of ["tab-step:-1", "tab-step:1"]) if (!n.includes(need)) return `missing ${need}: ${n}`;
    const c0 = (await page.ux()).home.compact;
    await page.padSet({ 3: true });
    await sleep(120);
    await page.padSet({});
    await sleep(120); // the hub sees a press as an up-to-down edge between polls: hold the release past a frame
    const on = await page.waitFor("the grid", (s) => s.home.compact !== c0, 3000);
    await page.shot(`${label}-hints-grid`);
    await page.padSet({ 3: true });
    await sleep(120);
    await page.padSet({});
    await sleep(120);
    const back = await page.waitFor("the cases again", (s) => s.home.compact === c0, 3000);
    return on.home.compact !== back.home.compact ? true : "Triangle did not toggle both ways";
  });
  await step("R1 on the pad steps to the next section, and its glyph is lit by the press", async () => {
    await page.padSet({ 5: true });
    await sleep(60);
    await page.padSet({});
    await page.waitFor("the next section", (s) => s.home.page !== "Home", 3000);
    await page.shot(`${label}-hints-r1`);
    return true;
  });
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** The shell around a running game, at one size: the launch sequence, then every section by every way back. */
async function journeyShell(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.goto(urlFor());
  const activate = async (name) => (mobile ? homeTap(page, name) : homeClick(page, name));
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
  await page.tapKey("x"); // cancel the countdown
  await step("Start disc plays the launch sequence (case opens, disc out, case recedes, tray, spin) and boots", async () => {
    await page.waitFor("the home screen", (s) => s.home.countdown === "cancelled", 5000);
    await activate("primary");
    const marks = [0.2, 0.5, 0.8, 1.1, 1.5, 2.0];
    let next = 0, seen = 0, brightest = 0;
    const start = Date.now();
    while (Date.now() - start < 40000) {
      const u = await page.ux();
      if (u.running && !u.menu_open) break;
      if (u.home.launch >= 0) {
        seen = Math.max(seen, u.home.launch);
        // The boot flash is a glow at the tray, never a wash over the whole screen.
        if (u.home.launch >= 1.85) brightest = Math.max(brightest, await page.meanLuma());
        if (next < marks.length && u.home.launch >= marks[next]) { await page.shot(`${label}-launch-${next + 1}`); next++; }
      }
      await sleep(120);
    }
    if (seen < 2.0) return `the sequence only reached ${seen} s`;
    if (args.trace) console.log("      brightest launch frame", brightest.toFixed(3));
    if (brightest > FLASH_LUMA_MAX) return `the boot flash washed the screen to ${brightest.toFixed(2)} (limit ${FLASH_LUMA_MAX})`;
    const u = await waitDemoRunning(page, 30000);
    return u.running ? true : "the game never started";
  });
  const open = async () => {
    if (mobile) {
      const t = (await page.ux()).touch?.ui.find((b) => b[0] === "menu");
      await page.tap(t[1] / size.dpr, t[2] / size.dpr);
    } else {
      await page.tapKey("Escape");
    }
    await page.waitFor("the shell", (s) => s.menu_open && s.home.page === "Home", 5000);
  };
  await step("the shell over the running game: the same nav, a Game section and the game's own case", async () => {
    await sleep(1500);
    await open();
    await sleep(800);
    await page.shot(`${label}-shell-home`);
    const u = await page.ux();
    const names = u.home.hits.map((h) => h[0]);
    for (const need of ["nav:Home", "nav:Game", "nav:Controls", "nav:Settings", "primary", "restart"]) if (!names.includes(need)) return `missing ${need}: ${names}`;
    if (u.running) return "the game is still running behind the shell";
    return homeLayoutProblem(u, size) ?? true;
  });
  await step("every section opens inside the shell and fits; each is left by pad or keyboard or touch", async () => {
    for (const section of ["Game", "Settings", "Controls"]) {
      await activate(`nav:${section}`);
      await page.waitFor(`the ${section} page`, (s) => s.home.page === section, 4000);
      await sleep(700);
      await page.shot(`${label}-shell-${section.toLowerCase()}`);
      const u = await page.ux();
      const problem = homeLayoutProblem(u, size);
      if (problem) return `${section}: ${problem}`;
      if (section === "Controls" && u.panel !== "controls") return "Controls opened outside the shell";
      // Back in the way this device has.
      if (mobile) await homeTap(page, "nav:Home");
      else if (section === "Game") await page.tapKey("g");
      else if (section === "Controls") await page.tapKey("Escape");
      else { await page.padConnect(); await sleep(200); await page.padSet({ 1: true }); await sleep(200); await page.padSet({}); }
      await page.waitFor(`Home after ${section}`, (s) => s.home.page === "Home" && s.menu_open, 4000);
    }
  });
  await step("the shell has no Debug page: Debug is a live panel of the game view", async () => {
    const u = await page.ux();
    const hits = u.home.hits.map((h) => h[0]);
    return hits.includes("nav:Debug") ? "Debug is still a tab of the shell" : true;
  });
  if (mobile) await step("the on-screen menu button answers mouse clicks as it does taps: a click pauses into the shell, a click on the header's close resumes", async () => {
    // A phone emulator, or a desktop with Touch controls On, sends mouse events: the controller must not
    // need a finger.
    await homeClick(page, "close");
    const resumed = await page.waitFor("resumed by a click", (s) => s.running && !s.menu_open, 5000).catch(() => null);
    if (!resumed) return `a mouse click on the shell's close button did not resume: ${JSON.stringify(await page.ux())}`;
    await sleep(600);
    const before = await page.ux();
    const t = before.touch?.ui.find((b) => b[0] === "menu");
    if (!t) return "no menu button while the game runs";
    await page.click(t[1] / size.dpr, t[2] / size.dpr);
    await page.waitFor("the shell opened by a click", (s) => s.menu_open && !s.running, 5000);
    await page.shot(`${label}-menu-by-mouse`);
    return (await page.ux()).machine === before.machine ? true : "the machine changed";
  });
  await step("Escape and the shell's close button leave the shell from Home and resume the same machine", async () => {
    const before = await page.ux();
    if (mobile) await homeTap(page, "close");
    else await page.tapKey("Escape");
    const after = await page.waitFor("resumed", (s) => s.running && !s.menu_open, 5000);
    return after.machine === before.machine ? true : "the machine changed";
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/**
 * The motion and honesty pass on the shell (home-polish3): the tab highlight slides, the pages arrive part by
 * part, every control answers a press, a phone does not claim a keyboard, the paused shell shows the running
 * game as its hero with nothing of the touch controller over it. (Debug is a live panel of the game
 * view since home-polish4, not a page of the shell.)
 */
async function journeyPolish(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.goto(urlFor());
  const activate = async (name) => (mobile ? homeTap(page, name) : homeClick(page, name));
  /** Samples of `read(state)` every ~25 ms while `during()` runs and for `tail` ms after it. */
  const sampled = async (read, during, tail = 700) => {
    const out = [];
    let live = true;
    const loop = (async () => { while (live) { const u = await page.ux(); if (u?.home) out.push(read(u)); await sleep(25); } })();
    await during();
    await sleep(tail);
    live = false;
    await loop;
    return out;
  };
  /**
   * Run a timed measurement and judge it only if the page drew enough frames while it ran: a busy machine
   * gives the page a few frames a second, and an animation measured over four frames proves nothing. The
   * measurement is repeated (bounded) until the page had the frames; `run` gets the attempt number.
   */
  const withFrames = async (run, minFrames = 12, tries = 5) => {
    let out;
    for (let i = 0; i < tries; i++) {
      const r0 = (await page.bench())?.redraws ?? 0;
      out = await run(i);
      const frames = ((await page.bench())?.redraws ?? 0) - r0;
      if (frames >= minFrames) return out;
      console.log(`      note: the page drew ${frames} frames while measuring (a busy machine); measuring again`);
      await sleep(600);
    }
    return out;
  };
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
  await step("a phone does not claim a keyboard until a key is pressed; a desktop has one", async () => {
    await activate("nav:Controls");
    await page.waitFor("Controls", (s) => s.home.page === "Controls", 4000);
    await sleep(700);
    await page.shot(`${label}-1-controls-devices`);
    const u = await page.ux();
    if (mobile && u.home.keyboard !== false) return `a phone claims a keyboard: ${u.home.keyboard}`;
    if (!mobile && u.home.keyboard !== true) return "the desktop lost its keyboard";
    if (mobile) {
      await page.tapKey("x");
      const after = await page.waitFor("a keyboard once a key was pressed", (s) => s.home.keyboard === true, 3000).catch(() => null);
      if (!after) return "a key press did not make the keyboard connected";
    }
  });
  await step("the tab highlight slides to the tab you pick, with a little spring, and settles", async () => {
    const series = await withFrames(async () => {
      await activate("nav:Home");
      await page.waitFor("Home", (s) => s.home.page === "Home" && s.home.nav_hl < 0.05, 8000);
      await sleep(900);
      const mid = sleep(150).then(() => page.shot(`${label}-2-nav-mid`));
      const out = await sampled((u) => u.home.nav_hl, async () => { await activate("nav:Controls"); }, 900);
      await mid;
      return out;
    });
    await page.shot(`${label}-2-nav-settled`);
    const end = series.at(-1);
    const between = series.filter((v) => v > 0.2 && v < 1.8);
    const peak = Math.max(...series);
    if (Math.abs(end - 2) > 0.02) return `the highlight ended at ${end}, not on Controls (2)`;
    if (between.length < 2) return `the highlight jumped instead of sliding: ${series.map((v) => v.toFixed(2)).join(" ")}`;
    if (peak > 2.5) return `the spring overshoots too far (${peak.toFixed(2)})`;
  });
  await step("a page arrives part by part: its clock restarts on every page and runs on", async () => {
    const ages = await withFrames(async (i) => {
      if (i > 0) { await activate("nav:Home"); await page.waitFor("Home", (s) => s.home.page === "Home", 8000); await sleep(600); }
      const mid = sleep(250).then(() => page.shot(`${label}-3-settings-arriving`));
      const out = await sampled((u) => u.home.page_age, async () => { await activate("nav:Settings"); }, 900);
      await mid;
      return out;
    });
    await page.shot(`${label}-3-settings`);
    if (!ages.some((v) => v < 0.2) || ages.at(-1) < 0.6) return `no staged arrival: ${ages.map((v) => v.toFixed(2)).join(" ")}`;
  });
  await step("a control answers a press at once (a glow rises under it while it is held)", async () => {
    await activate("nav:Home");
    await page.waitFor("Home", (s) => s.home.page === "Home", 4000);
    await sleep(500);
    const [x, y] = await homeHit(page, "nav:Settings");
    if (mobile) await page.touch("touchStart", [[x, y]]);
    else { await page.mouse("mouseMoved", x, y); await sleep(40); await page.mouse("mousePressed", x, y); }
    // The glow rises over a few frames: wait for the page to have drawn them (bounded), not for a fixed time.
    const lit = await page.waitFor("the glow under the held control", (s) => s.home.press > 0.5, 8000).catch(() => null);
    const held = lit ? lit.home.press : (await page.ux()).home.press;
    await page.shot(`${label}-4-press`);
    if (mobile) await page.touch("touchEnd", []);
    else await page.mouse("mouseReleased", x, y);
    const drained = await page.waitFor("the glow draining", (s) => s.home.press < 0.05, 8000).catch(() => null);
    const gone = drained ? drained.home.press : (await page.ux()).home.press;
    await page.waitFor("Settings after the press", (s) => s.home.page === "Settings", 4000);
    if (!(held > 0.5)) return `no press feedback while held (${held})`;
    if (!(gone < 0.05)) return `the glow did not drain (${gone})`;
  });
  if (mobile) await step("the Controls selector is a sliding segmented control", async () => {
    await activate("nav:Controls");
    await page.waitFor("Controls", (s) => s.home.page === "Controls", 4000);
    await sleep(900);
    const series = await withFrames(async (i) => {
      if (i > 0) { await activate("ctl:tab:Devices"); await sleep(900); }
      const mid = sleep(150).then(() => page.shot(`${label}-5-controls-selector-mid`));
      const out = await sampled((u) => u.home.controls.seg, async () => { await activate("ctl:tab:Touch"); }, 900);
      await mid;
      return out;
    });
    await page.shot(`${label}-5-controls-touch`);
    const end = series.at(-1);
    const between = series.filter((v) => v > 0.2 && v < 1.8);
    if (Math.abs(end - 2) > 0.02) return `the selector ended at ${end}`;
    if (between.length < 2) return `the selector jumped: ${series.map((v) => v.toFixed(2)).join(" ")}`;
  });
  await step("start the demo disc", async () => {
    await activate("nav:Home");
    await page.waitFor("Home", (s) => s.home.page === "Home", 4000);
    await sleep(500);
    await activate("primary");
    await waitDemoRunning(page, 90000);
    await sleep(1500);
  });
  const openShell = async () => {
    if (mobile) {
      const t = (await page.ux()).touch?.ui.find((b) => b[0] === "menu");
      if (!t) throw new Error("no touch menu button while the game runs");
      await page.tap(t[1] / size.dpr, t[2] / size.dpr);
    } else await page.tapKey("Escape");
    await page.waitFor("the shell", (s) => s.menu_open && s.home.page === "Home", 5000);
    await sleep(900);
  };
  await step("the paused shell: the game is the hero (Running, Resume, Start over) and nothing of the controller overlaps", async () => {
    await openShell();
    await page.shot(`${label}-6-shell-hero`);
    const u = await page.ux();
    const names = u.home.hits.map((h) => h[0]);
    for (const need of ["primary", "restart", "nav:Game"]) if (!names.includes(need)) return `missing ${need}: ${names}`;
    const hero = u.home.cases[0];
    if (!hero || hero[0] !== "PSoXide Demo Disc") return `the first case is not the running game: ${JSON.stringify(hero)}`;
    if (!/pick up where you left off/i.test(hero[1])) return `the hero line is "${hero[1]}"`;
    if (u.home.case !== 0) return `the shell opened on case ${u.home.case}, not the running game`;
    if (mobile) {
      if ((u.touch?.ui ?? []).length) return `the controller's buttons are still laid out over the shell: ${JSON.stringify(u.touch.ui.map((b) => b[0]))}`;
      if (!names.includes("close")) return `no way back to the game in the header: ${names}`;
    }
    const problem = homeLayoutProblem(u, size);
    if (problem) return problem;
    // The other pages over the game keep clear of the header's close button too.
    for (const section of ["Controls", "Settings"]) {
      await activate(`nav:${section}`);
      await page.waitFor(`${section}`, (s) => s.home.page === section, 4000);
      await sleep(700);
      const p2 = homeLayoutProblem(await page.ux(), size);
      if (p2) return `${section}: ${p2}`;
    }
    await activate("nav:Home");
    await page.waitFor("Home", (s) => s.home.page === "Home", 4000);
  });
  if (mobile) await step("the phone's tab bar is centred in the viewport, with the same margin either side (framed or not)", async () => {
    const u = await page.ux();
    const tabs = u.home.hits.filter((h) => /^nav:/.test(h[0]));
    if (tabs.length !== 4) return `expected four tabs (Home, Game, Settings, Controls): ${tabs.map((h) => h[0])}`;
    const left = Math.min(...tabs.map((h) => h[1])), right = Math.max(...tabs.map((h) => h[3]));
    const offset = (left + right) / 2 - size.w / 2;
    if (Math.abs(offset) > 1.5) return `the bar spans ${left.toFixed(1)}..${right.toFixed(1)} of ${size.w}: ${offset.toFixed(1)} px off centre`;
    if (size.framed && u.home.framed !== true) return "the player does not know it is framed";
  });
  /** Back to the game from the shell: the header's close button (a build before it had the controller's menu button). */
  const closeShell = async () => {
    const u = await page.ux();
    if (u.home.hits.some((h) => h[0] === "close")) return homeTap(page, "close");
    const t = u.touch?.ui.find((b) => b[0] === "menu");
    if (t) await page.tap(t[1] / size.dpr, t[2] / size.dpr);
  };
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

// ---------------------------------------------------------------- debug panel, save states, centring

/**
 * Does a coast slow down smoothly? `trace` is [{t ms, p px}]. The readings come at the harness's pace and the page
 * draws a frame when it can, so single intervals jitter: the check is that the speed over the last three
 * intervals is clearly lower than over the first three and that no interval is faster than the first. Returns a
 * message or null.
 */
function coastProblem(trace) {
  const speeds = trace.slice(1).map((s, i) => ((s.p - trace[i].p) / Math.max(1, s.t - trace[i].t)) * 1000);
  const mean = (v) => v.reduce((a, b) => a + b, 0) / v.length;
  const early = mean(speeds.slice(0, 3)), late = mean(speeds.slice(-3));
  const text = speeds.map((x) => x.toFixed(0)).join(",");
  if (!(late < early * 0.8)) return `it did not slow down: ${text} px/s`;
  if (speeds.some((v) => v > Math.max(...speeds.slice(0, 2)) * 1.5 + 40)) return `it sped up while coasting: ${text} px/s`;
  return null;
}

const SILENT = `try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`;

/**
 * Start the demo disc from a fresh page: cancel the countdown, press Start disc, wait for it to run. A phone does
 * it with fingers: a key press would tell the player there is a keyboard and hide the on-screen controller.
 */
async function bootDemo(page, size) {
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
  if (size.mobile) {
    const [x, y] = await emptyPoint(page, size);
    await page.tap(x, y);
  } else await page.tapKey("x");
  await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 10000);
  await page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
  if (size.mobile) await homeTap(page, "primary");
  else await page.tapKey("Enter");
  await waitDemoRunning(page, 90000);
  // The launch sequence hands over to the game a moment after it runs: wait until the shell is gone.
  await page.waitFor("the shell gone", (x) => x.home.cover === 0, 20000);
  // The disc streams in: the screen stays black until the game has drawn its first picture.
  await page.waitFor("the game's first picture", (x) => x.display_lit === undefined || x.display_lit > 100, 120000);
  await sleep(1000);
  if (args.trace) {
    const seen = [];
    for (let i = 0; i < 40; i++) { { const u = await page.ux(); seen.push(u.display.join(",") + " lit=" + (u.display_lit > 100 ? "many" : u.display_lit)); } await sleep(50); }
    console.log("      display areas over 2 s:", [...new Set(seen)].map((d) => `${d} x${seen.filter((v) => v === d).length}`).join(" | "));
  }
}

/** A return visit: wait for Home with the disc ready and start it the way this device does. */
async function startAgain(page, size) {
  await page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
  if (size.mobile) await homeTap(page, "primary");
  else await page.tapKey("Enter");
  await waitDemoRunning(page, 90000);
  await page.waitFor("the shell gone", (x) => x.home.cover === 0, 20000);
  await page.waitFor("the game's first picture", (x) => x.display_lit === undefined || x.display_lit > 100, 120000);
  await sleep(1000);
}

/** A touch button of the on-screen controller, by name, in css px. */
async function touchButton(page, size, name) {
  const t = (await page.ux())?.touch?.ui.find((b) => b[0] === name);
  if (!t) throw new Error(`no ${name} button on the touch controller`);
  return [t[1] / size.dpr, t[2] / size.dpr];
}

/**
 * The live debug panel (home-polish4): it opens beside or under the running game, the game never pauses, the
 * controller is laid out around it, and it goes away again.
 */
async function journeyDebug(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(SILENT);
  await page.goto(urlFor());
  await bootDemo(page, size);
  const toggle = async () => {
    if (mobile) { const [x, y] = await touchButton(page, size, "debug"); await page.tap(x, y); }
    else await page.tapKey("F3");
  };
  /** The controller's drawn rects in css px; `padOnly` leaves out the pause, save, load, debug and menu buttons. */
  const padRects = async (padOnly = false) => ((await page.ux()).touch?.rects ?? []).filter((r) => !padOnly || r[0] !== 0).map((r) => r.slice(1).map((v) => v / size.dpr));
  const before = await page.ux();
  const gameBefore = before.game;
  const padBefore = mobile ? JSON.stringify(await padRects()) : "";
  await page.shot(`${label}-1-closed`);
  if (mobile && size.w < size.h) await step("portrait: the controls are one packed block right under the interface row, the room left is below them", async () => {
    const rects = (await page.ux()).touch.rects.map((r) => [r[0], ...r.slice(1).map((v) => v / size.dpr)]);
    const uiBottom = Math.max(...rects.filter((r) => r[0] === 0).map((r) => r[4]));
    const shoulder = rects.find((r) => r[0] === PS1.L1);
    if (!shoulder) return "no L1";
    if (shoulder[2] - uiBottom > 30) return `L1 starts ${(shoulder[2] - uiBottom).toFixed(0)} px under the interface row`;
    if (shoulder[2] < uiBottom) return "the shoulders are over the interface row";
  });
  await step("the debug button opens the panel and the game keeps running at full speed", async () => {
    const a = await page.ux();
    await sleep(900);
    const rate0 = (await page.ux()).cpu_tick - a.cpu_tick;
    const contents0 = a.debug.contents;
    await toggle();
    // Every frame the panel shows has its contents laid out: the first one with any of it showing
    // is not a blank box.
    const first = await page.waitFor("the panel starting to show", (s) => s.debug.slide > 0.001, 4000);
    if (first.debug.contents <= contents0) return `the panel showed (slide ${first.debug.slide}) before its contents were laid out`;
    const open = await page.waitFor("the panel open", (s) => s.debug.open && s.debug.slide > 0.99, 4000);
    await sleep(900);
    await page.shot(`${label}-2-open`);
    const later = await page.ux();
    if (!later.running || later.menu_open) return `the game paused or the shell opened: ${JSON.stringify({ r: later.running, m: later.menu_open })}`;
    if (later.machine !== before.machine) return "the machine changed";
    const ran = later.cpu_tick - open.cpu_tick;
    // The same stretch of time with the panel away is the yardstick: the panel may cost a little, never the game.
    if (ran < rate0 * 0.6) return `the guest ran ${ran} cycles in 0.9 s with the panel open, ${rate0} without`;
  });
  await step("the panel sits where it should and nothing of the controller is under it", async () => {
    const u = await page.ux();
    if (mobile) { const problem = gameProblems(u, size.dpr); if (problem) return problem; }
    const [x0, y0, x1, y1] = u.debug.rect;
    const landscape = size.w >= size.h;
    if (x0 < -0.5 || y0 < -0.5 || x1 > size.w + 0.5 || y1 > size.h + 0.5) return `the panel is off screen: ${u.debug.rect}`;
    if (landscape) {
      if (x1 < size.w - 60) return `the panel is not at the right edge: ${u.debug.rect}`;
      if (x1 - x0 < 240 || x1 - x0 > size.w * 0.6) return `the panel is ${x1 - x0} wide in a ${size.w} window`;
      if (mobile) for (const [rx0, ry0, rx1] of (await padRects()).map((r) => [r[0], r[1], r[2]])) if (rx1 > x0 + 1) return `a control reaches ${rx1} into the panel at ${x0}`;
    } else {
      if (y1 - y0 < 110) return `the strip is only ${y1 - y0} tall`;
      if (y1 < size.h - 2) return `the strip does not reach the bottom of the window: ${u.debug.rect}`;
      if (mobile) {
        // Under the controls, in the room the pad panel leaves: never over a button.
        const pads = await padRects();
        const lowest = Math.max(...pads.map((r) => r[3]));
        if (y0 < lowest - 1) return `the strip (from ${y0}) runs into the controls (down to ${lowest})`;
      }
    }
    // The picture keeps its whole width and place with the strip open (portrait) or gives up only the
    // column's share (landscape).
    if (!landscape) {
      const g = u.game;
      if (Math.abs((g[2] - g[0]) - (gameBefore[2] - gameBefore[0])) > 0.6 || Math.abs(g[1] - gameBefore[1]) > 0.6) {
        return `the picture changed from [${gameBefore}] to [${g}] when the strip opened`;
      }
      if (g[2] - g[0] < size.w * 0.97) return `the picture is only ${g[2] - g[0]} wide in a ${size.w} window`;
    }
  });
  const portrait = mobile && size.w < size.h;
  /** Drag the sheet's handle (portrait) by `dy` css px and let go after a rest. */
  const dragHandle = async (dy) => {
    const u = await page.ux();
    const x = size.w / 2, y0 = u.debug.rect[1] + 9;
    await page.touch("touchStart", [[x, y0]]);
    for (let i = 1; i <= 10; i++) { await sleep(16); await page.touch("touchMove", [[x, y0 + (dy * i) / 10]]); }
    await sleep(160);
    await page.touch("touchEnd", []);
    await sleep(500);
  };
  await step("the summary is one line and the tabs and content get the panel", async () => {
    const u = await page.ux();
    const [top, bottom] = [u.debug.scroll[3], u.debug.scroll[4]];
    const [, y0, , y1] = u.debug.rect;
    if (!(bottom > top)) return `no content view: ${u.debug.scroll}`;
    const share = (bottom - top) / (y1 - y0);
    await page.shot(`${label}-4-compact`);
    // The phone's panel keeps one 32 px line for the readings; a desktop sidebar keeps its big numbers.
    if (mobile && (share < 0.4 || bottom - top < 125)) return `the content view is ${(share * 100).toFixed(0)}% of the panel (${bottom - top} of ${y1 - y0} px)`;
    if (!mobile && bottom - top < 150) return `the content view is only ${bottom - top} px`;
  });
  if (portrait) await step("portrait: the sheet is up over the controller, under the interface row, the picture whole above it", async () => {
    await page.waitFor("the sheet up", (s) => s.debug.sheet > 0.99, 4000);
    const u = await page.ux();
    if (u.touch.buttons.length || u.touch.sticks.length) return "the pad's controls are still there under the sheet";
    if (u.touch.ui.length < 5) return "the interface row is gone";
    const uiBottom = Math.max(...u.touch.rects.filter((r) => r[0] === 0).map((r) => r[4] / size.dpr));
    const [, y0, , y1] = u.debug.rect;
    if (y0 < uiBottom - 1 || y0 > uiBottom + 16) return `the sheet starts at ${y0}, the interface row ends at ${uiBottom}`;
    if (y1 - y0 < size.h * 0.3) return `the sheet is only ${y1 - y0} px of a ${size.h} px window`;
    const problem = gameProblems(u, size.dpr);
    if (problem) return problem;
    await page.shot(`${label}-5-sheet-up`);
  });
  // Momentum scrolling of the panel's content (the shell's lists have their own journey, journeyMomentum).
  const scrollNow = async () => (await page.ux()).debug.scroll;
  const contentPoint = async (frac) => {
    const u = await page.ux();
    const [top, bottom] = [u.debug.scroll[3], u.debug.scroll[4]];
    const [x0, , x1] = u.debug.rect;
    return [Math.round((x0 + x1) / 2), Math.round(top + (bottom - top) * frac)];
  };
  /** A finger drag over the content from fraction `from` to `to` of its height in `steps` frames, lifted at speed. */
  const flickContent = async (from, to, steps = 4, rest = 0) => {
    const [x, y0] = await contentPoint(from);
    const [, y1] = await contentPoint(to);
    const t0 = Date.now();
    await page.touch("touchStart", [[x, y0]]);
    for (let i = 1; i <= steps; i++) { await sleep(16); await page.touch("touchMove", [[x, y0 + ((y1 - y0) * i) / steps]]); }
    // The finger's own speed in css px per second, as the harness really delivered it.
    lastFlickSpeed = (Math.abs(y1 - y0) / Math.max(1, Date.now() - t0)) * 1000;
    if (rest) await sleep(rest);
    await page.touch("touchEnd", []);
  };
  let lastFlickSpeed = 0;
  const toTop = async () => {
    for (let i = 0; i < 30 && (await scrollNow())[0] > 0.5; i++) { await flickContent(0.2, 0.9, 8, 140); await sleep(400); }
  };
  if (mobile) await step("a flick on the panel's content keeps coasting after the finger lifts and slows to a stop", async () => {
    // The harness delivers touch events at the page's pace; a flick that was delivered slowly says nothing about
    // momentum, so a slow one is tried again (a fast one is what a finger does).
    let start;
    for (let attempt = 0; attempt < 6; attempt++) {
      await toTop();
      await sleep(500);
      start = await scrollNow();
      if (start[1] < 300) return `the content is only ${start[1]} px past its view, nothing to fling`;
      await flickContent(0.95, 0.15, 3);
      if (lastFlickSpeed >= 300) break;
    }
    if (lastFlickSpeed < 300) return `the harness could not deliver a flick faster than ${lastFlickSpeed.toFixed(0)} px/s`;
    const lifted = await scrollNow();
    const trace = [{ t: Date.now(), p: lifted[0] }];
    for (let i = 0; i < 10; i++) { await sleep(70); trace.push({ t: Date.now(), p: (await scrollNow())[0] }); }
    const samples = trace.map((x) => x.p);
    const moved = samples[samples.length - 1] - lifted[0];
    // A flick of v px/s coasts about v/2 px; allow for what was already travelled before the first reading.
    const wanted = Math.min(lastFlickSpeed / 2 - 150, 400) * 0.5;
    if (moved < Math.max(40, wanted)) return `it only travelled ${moved.toFixed(0)} px after the finger lifted a ${lastFlickSpeed.toFixed(0)} px/s flick (${samples.map((v) => v.toFixed(0))})`;
    const slow = coastProblem(trace);
    if (slow) return slow;
    const end = await page.waitFor("the coast to end", (x) => x.debug.scroll[2] === 0, 6000);
    await page.shot(`${label}-7-after-fling`);
    if (end.debug.scroll[0] <= 50) return `it ended at ${end.debug.scroll[0]}`;
  });
  if (mobile) await step("a finger put down on a coasting panel stops it where it is and presses nothing", async () => {
    await toTop();
    await sleep(500);
    await flickContent(0.9, 0.2);
    await sleep(160);
    const before = (await scrollNow())[0];
    if ((await scrollNow())[2] !== 1) return "it was not coasting";
    const [x, y] = await contentPoint(0.3);
    await page.tap(x, y, 60);
    const at = (await scrollNow())[0];
    await sleep(500);
    const later = await scrollNow();
    if (Math.abs(later[0] - at) > 2) return `it kept going after the touch: ${at} then ${later[0]}`;
    if (at <= before) return `the touch did not even find it moving: ${before} then ${at}`;
    if (later[2] !== 0) return "still flagged as moving";
  });
  if (mobile) await step("pulled past the top the content stretches under the finger and springs back; nothing stays stretched", async () => {
    await toTop();
    await sleep(600);
    const [x, y0] = await contentPoint(0.2);
    await page.touch("touchStart", [[x, y0]]);
    for (let i = 1; i <= 12; i++) { await sleep(16); await page.touch("touchMove", [[x, y0 + 12 * i]]); }
    await sleep(120);
    const held = (await scrollNow())[0];
    await page.shot(`${label}-8-stretched`);
    await page.touch("touchEnd", []);
    if (held > -15) return `held 144 px down at the top, the position is ${held} (no stretch)`;
    if (held < -0.4 * 400) return `stretched too far: ${held}`;
    const back = [];
    for (let i = 0; i < 12; i++) { await sleep(60); back.push((await scrollNow())[0]); }
    if (back.some((v) => v > 0.5)) return `crossed the top on the way back: ${back}`;
    const end = await scrollNow();
    if (end[0] !== 0 || end[2] !== 0) return `did not settle at the top: ${end}`;
  });
  if (!mobile) await step("the mouse wheel scrolls the content and stops at the ends", async () => {
    const [x, y] = await contentPoint(0.5);
    await page.mouse("mouseMoved", x, y);
    const before = (await scrollNow())[0];
    for (let i = 0; i < 6; i++) await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x, y, deltaX: 0, deltaY: 120 }, page.top);
    await sleep(500);
    const after = await scrollNow();
    if (!(after[0] > before + 100)) return `six wheel notches moved ${before} to ${after[0]}`;
    for (let i = 0; i < 60; i++) await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x, y, deltaX: 0, deltaY: -400 }, page.top);
    await sleep(600);
    const top = await scrollNow();
    if (top[0] !== 0) return `scrolling up for a long time left it at ${top[0]}`;
  });
  if (portrait) await step("portrait: dragging the handle down brings the controller back under a short strip, dragging up covers it again", async () => {
    await dragHandle(260);
    const down = await page.waitFor("the sheet down", (s) => s.debug.sheet < 0.01, 4000);
    if (!down.touch.buttons.length) return "the controller did not come back";
    await page.shot(`${label}-6-sheet-down`);
    await dragHandle(-260);
    await page.waitFor("the sheet up again", (s) => s.debug.sheet > 0.99, 4000);
    if ((await page.ux()).touch.buttons.length) return "the pad shows through the sheet";
    // A tap on the handle flips it.
    const u = await page.ux();
    await page.tap(size.w / 2, u.debug.rect[1] + 9);
    await page.waitFor("the sheet down by a tap", (s) => s.debug.sheet < 0.01, 4000);
    await page.tap(size.w / 2, (await page.ux()).debug.rect[1] + 9);
    await page.waitFor("the sheet up by a tap", (s) => s.debug.sheet > 0.99, 4000);
    await dragHandle(260);
    await page.waitFor("the sheet down for the controller", (s) => s.debug.sheet < 0.01, 4000);
  });
  if (mobile) await step("the controller still plays the game while the panel is open", async () => {
    const u = await page.ux();
    const cross = u.touch.buttons.find((b) => b[0] === 0x4000);
    if (!cross) return "no Cross on the controller";
    await page.touch("touchStart", [[cross[1] / size.dpr, cross[2] / size.dpr]]);
    await sleep(250);
    const t = await page.touchState();
    await page.touch("touchEnd", []);
    return t && t.port1_mask & PS1.CROSS ? true : JSON.stringify(t);
  });
  else await step("the keyboard still plays the game while the panel is open", async () => {
    await page.key("f", true, false);
    await sleep(250);
    const t = await page.touchState();
    await page.key("f", false, true);
    return t && t.port1_mask & PS1.CROSS ? true : JSON.stringify(t);
  });
  await step("the panel has no shell page behind it: the shell is closed and Debug is not a tab", async () => {
    const u = await page.ux();
    return !u.menu_open && !u.home.hits.some((h) => h[0] === "nav:Debug") ? true : "the shell is up or has a Debug tab";
  });
  await step("closing the panel slides it away, the game keeps running and the controller returns to its place", async () => {
    await toggle();
    await page.waitFor("the panel gone", (s) => !s.debug.open && s.debug.slide < 0.01, 4000);
    await sleep(500);
    await page.shot(`${label}-3-closed-again`);
    const u = await page.ux();
    if (!u.running || u.menu_open) return "the game paused";
    if (mobile && JSON.stringify(await padRects()) !== padBefore) return "the controller did not return to its place";
  });
  if (!mobile) await step("a gamepad: Select+R3 shows the panel and again hides it", async () => {
    await page.padConnect();
    await sleep(400);
    await page.padSet({ 8: true });
    await sleep(120);
    await page.padSet({ 8: true, 11: true });
    await page.waitFor("the panel opened by the pad", (s) => s.debug.open, 4000);
    await page.padSet({});
    await sleep(300);
    await page.padSet({ 8: true });
    await sleep(120);
    await page.padSet({ 8: true, 11: true });
    await page.waitFor("the panel closed by the pad", (s) => !s.debug.open, 4000);
    await page.padSet({});
    const t = await page.touchState();
    return t.port1_mask & PS1.R3 ? "R3 reached the guest" : true;
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** Rewrite the stored save of a slot in the page's IndexedDB: `change` names which field of the trailer to alter. */
async function mutateSave(page, gameId, slot, change) {
  return page.eval(`(async () => {
    const open = () => new Promise((res, rej) => { const r = indexedDB.open('psoxide-fs', 2); r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error); });
    const get = (db, k) => new Promise((res, rej) => { const q = db.transaction('savestates').objectStore('savestates').get(k); q.onsuccess = () => res(q.result); q.onerror = () => rej(q.error); });
    const put = (db, k, v) => new Promise((res, rej) => { const t = db.transaction('savestates', 'readwrite'); t.objectStore('savestates').put(v, k); t.oncomplete = () => res(); t.onerror = () => rej(t.error); });
    const varint = (b, i) => { let v = 0n, s = 0n, n = 0; for (;;) { const x = b[i + n++]; v |= BigInt(x & 127) << s; s += 7n; if (!(x & 128)) break; } return [v, n]; };
    const enc = (v) => { const out = []; do { let x = Number(v & 127n); v >>= 7n; if (v) x |= 128; out.push(x); } while (v); return out; };
    const db = await open();
    const rec = await get(db, 'slot:${gameId}:${slot}');
    const bytes = rec.bytes;
    const tail = bytes.length - 12;
    const metaLen = new DataView(bytes.buffer, bytes.byteOffset + tail, 4).getUint32(0, true);
    const metaStart = tail - metaLen;
    const meta = Array.from(bytes.slice(metaStart, tail));
    // meta_version, game_id, title, disc_hash, created_at, cpu_tick, format_version, creator, thumb
    let i = 0, n;
    [, n] = varint(meta, i); i += n;
    for (let k = 0; k < 2; k++) { let len; [len, n] = varint(meta, i); i += n + Number(len); }
    const field = (name) => { const at = i; const [v, w] = varint(meta, at); i += w; return { name, at, w, v }; };
    const disc = field('disc'); field('created'); field('tick'); const fmt = field('format');
    const target = '${change}' === 'disc' ? disc : fmt;
    const value = '${change}' === 'disc' ? disc.v ^ 0x5a5a5a5an : fmt.v + 5n;
    const newMeta = meta.slice(0, target.at).concat(enc(value), meta.slice(target.at + target.w));
    const out = new Uint8Array(metaStart + newMeta.length + 12);
    out.set(bytes.slice(0, metaStart), 0);
    out.set(newMeta, metaStart);
    new DataView(out.buffer).setUint32(metaStart + newMeta.length, newMeta.length, true);
    out.set(bytes.slice(tail + 4), metaStart + newMeta.length + 4);
    await put(db, 'slot:${gameId}:${slot}', { bytes: out });
    await put(db, 'slotmeta:${gameId}:${slot}', new Uint8Array(newMeta));
    return true;
  })()`);
}

/** Cut a stored save short, as a failed write or a bad disk would. */
async function truncateSave(page, gameId, slot) {
  return page.eval(`(async () => {
    const db = await new Promise((res, rej) => { const r = indexedDB.open('psoxide-fs', 2); r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error); });
    const rec = await new Promise((res, rej) => { const q = db.transaction('savestates').objectStore('savestates').get('slot:${gameId}:${slot}'); q.onsuccess = () => res(q.result); q.onerror = () => rej(q.error); });
    const bytes = rec.bytes;
    // Keep the trailer (so the sheet still lists it) but lose most of the machine state.
    const tail = bytes.length - 12;
    const metaLen = new DataView(bytes.buffer, bytes.byteOffset + tail, 4).getUint32(0, true);
    const keep = bytes.slice(tail - metaLen);
    const out = new Uint8Array(1000 + keep.length);
    out.set(bytes.slice(0, 1000), 0);
    out.set(keep, 1000);
    await new Promise((res, rej) => { const t = db.transaction('savestates', 'readwrite'); t.objectStore('savestates').put({ bytes: out }, 'slot:${gameId}:${slot}'); t.oncomplete = () => res(); t.onerror = () => rej(t.error); });
    return true;
  })()`);
}

/**
 * Save states (home-polish4): a sheet of slots with pictures; save, overwrite with a question, load, delete;
 * survive a reload; refuse another disc, another format or a damaged save without touching the game.
 */
async function journeySlots(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(SILENT);
  await page.goto(urlFor());
  await bootDemo(page, size);
  const gameId = (await page.ux()).loaded_id;
  const slots = async () => (await page.ux()).slots;
  const hit = async (name) => {
    const h = (await slots()).hits.find((x) => x[0] === name);
    if (!h) throw new Error(`no "${name}" on the sheet: ${(await slots()).hits.map((x) => x[0])}`);
    return [(h[1] + h[3]) / 2, (h[2] + h[4]) / 2];
  };
  const press = async (name) => {
    // A short screen shows one row of the grid: scroll the sheet until the control is in view.
    let [x, y] = await hit(name);
    for (let i = 0; i < 14 && (y > size.h - 70 || y < 100) && !/^(mode|ask|file):/.test(name); i++) {
      await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: size.w / 2, y: size.h / 2, deltaX: 0, deltaY: y > size.h / 2 ? 140 : -140 }, page.top);
      await sleep(300);
      [x, y] = await hit(name);
    }
    await (mobile ? page.tap(x, y) : page.click(x, y));
    await sleep(250);
  };
  /** Open the sheet the way this device does. */
  const openSheet = async (mode) => {
    const now = await page.ux();
    if (now.menu_open && now.home.page === "Slots") {
      if (now.slots.mode !== mode) await press(`mode:${mode}`);
      return;
    }
    if (mobile) { const [x, y] = await touchButton(page, size, mode === "Save" ? "save" : "load"); await page.tap(x, y); }
    else {
      // Esc, the Game page, the row.
      await page.tapKey("Escape");
      await page.waitFor("the shell", (s) => s.menu_open, 4000);
      await menuSelect(page, "Game", mode === "Save" ? "Save state" : "Load state");
    }
    await page.waitFor("the slot sheet", (s) => s.home.page === "Slots" && s.menu_open, 5000);
    await sleep(900);
    const u = await slots();
    if (u.mode !== mode) await press(`mode:${mode}`);
  };
  /** Leave the sheet and go back to the running game. */
  const resume = async () => {
    for (let i = 0; i < 5 && (await page.ux()).menu_open; i++) {
      const u = await page.ux();
      if (mobile && u.home.hits.some((h) => h[0] === "close")) await homeTap(page, "close");
      else await page.tapKey("Escape");
      await sleep(350);
    }
    await page.waitFor("the game running", (s) => s.running && !s.menu_open, 6000);
  };
  const status = async () => (await page.ux()).status;
  const tickNow = async () => (await page.ux()).cpu_tick;

  let savedTick = 0, stamp1 = 0, sig1 = "";
  await step("the sheet opens over the paused game and every slot starts empty", async () => {
    await openSheet("Save");
    await page.shot(`${label}-1-sheet-empty`);
    const u = await page.ux();
    if (u.running) return "the game still runs behind the sheet";
    if (u.slots.cards.length !== 9) return `expected 9 slots, got ${u.slots.cards.length}`;
    if (u.slots.cards.some((c) => c.stamp !== 0)) return "a fresh game has saves";
    if (u.home.hits.some((h) => h[0] === "nav:Debug")) return "a Debug tab";
    if (!u.home.hits.some((h) => h[0] === "nav:Game")) return "no Game tab";
    savedTick = u.cpu_tick;
  });
  await step("saving into Slot 1 stores it with a picture, the time and a toast", async () => {
    await press("slot:1");
    const u = await page.waitFor("the save landing", (s) => s.slots.cards[1].stamp > 0, 6000);
    stamp1 = u.slots.cards[1].stamp;
    await page.shot(`${label}-2-sheet-saved`);
    if (!u.slots.cards[1].thumb) return "no thumbnail";
    if (u.slots.cards[1].luma < 2) return `the picture is black (luma ${u.slots.cards[1].luma})`;
    sig1 = u.slots.cards[1].sig;
    if (u.cpu_tick !== savedTick) return "the machine moved while the sheet was open";
    return /Saved to Slot 1/.test(u.status) ? true : `status "${u.status}"`;
  });
  await step("a second save into the same slot asks first; Cancel keeps the old one, Replace writes the new", async () => {
    await sleep(1200);
    await press("slot:1");
    await page.waitFor("the question", (s) => s.slots.ask === "overwrite:1", 3000);
    await page.shot(`${label}-3-overwrite-asks`);
    await press("ask:cancel");
    await page.waitFor("the question gone", (s) => s.slots.ask === null, 3000);
    if ((await slots()).cards[1].stamp !== stamp1) return "Cancel changed the save";
    await press("slot:1");
    await page.waitFor("the question again", (s) => s.slots.ask === "overwrite:1", 3000);
    await press("ask:confirm");
    const u = await page.waitFor("the new save", (s) => s.slots.cards[1].stamp > stamp1, 6000);
    stamp1 = u.slots.cards[1].stamp;
  });
  await step("leaving the sheet resumes the game; it runs on past the saved moment", async () => {
    await resume();
    await sleep(2500);
    const t = await tickNow();
    return t > savedTick + 15_000_000 ? true : `only ${t - savedTick} cycles on`;
  });
  await step("Load shows the grid; choosing Slot 1 puts the machine back where it was saved, and the game runs on", async () => {
    const beforeLoad = await tickNow();
    await openSheet("Load");
    await page.shot(`${label}-4-sheet-load`);
    const paused = await tickNow();
    await press("slot:1");
    const u = await page.waitFor("the load", (s) => s.running && !s.menu_open && /Loaded Slot 1/.test(s.status), 8000);
    await sleep(300);
    const after = await tickNow();
    // The slot was saved at `paused - 0` cycles of this sheet's pause; the machine goes back to it.
    if (after > paused + 40_000_000 && after > savedTick + 40_000_000) return `after the load the machine is at ${after}, not back near ${savedTick}..${paused}`;
    if (after > beforeLoad) return "the machine did not go back";
    await page.shot(`${label}-5-after-load`);
  });
  await step("quick save and quick load: F5 and F7 on a keyboard, or the Quick card on a phone", async () => {
    if (!mobile) {
      await page.tapKey("F5");
      await page.waitFor("the quick save toast", (s) => /Quick saved/.test(s.status), 6000);
      const saved = await tickNow();
      await sleep(2000);
      await page.tapKey("F7");
      await page.waitFor("the quick load toast", (s) => /Quick loaded/.test(s.status), 8000);
      const after = await tickNow();
      if (after > saved + 40_000_000) return `F7 left the machine at ${after}, saved at ${saved}`;
      const quick = (await slots()).cards[0];
      if (!quick.thumb || quick.luma < 2) return `the quick save's picture is black: ${JSON.stringify(quick)}`;
    } else {
      await openSheet("Save");
      await press("slot:0");
      await page.waitFor("the quick save", (s) => s.slots.cards[0].stamp > 0, 6000);
      await resume();
      await openSheet("Load");
      await press("slot:0");
      await page.waitFor("the quick load", (s) => s.running && /Quick loaded/.test(s.status), 8000);
    }
  });
  if (!mobile) await step("a gamepad: Select+R1 quick-saves and Select+L1 quick-loads, with toasts", async () => {
    await page.padConnect();
    await sleep(400);
    const chord = async (b) => { await page.padSet({ 8: true }); await sleep(120); await page.padSet({ 8: true, [b]: true }); await sleep(250); await page.padSet({}); await sleep(300); };
    await chord(5);
    await page.waitFor("quick saved", (s) => /Quick saved/.test(s.status), 6000);
    await chord(4);
    await page.waitFor("quick loaded", (s) => /Quick loaded/.test(s.status), 8000);
  });
  await step("a slot's save survives a reload of the page", async () => {
    await page.goto(urlFor());
    await page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
    await startAgain(page, size);
    await openSheet("Load");
    await page.shot(`${label}-6-after-reload`);
    const u = await page.ux();
    const c = u.slots.cards[1];
    if (c.stamp !== stamp1) return `Slot 1 reads ${c.stamp} after the reload, not ${stamp1}`;
    if (!c.thumb || c.problem) return `Slot 1 after the reload: ${JSON.stringify(c)}`;
    if (c.sig !== sig1) return `the picture changed across the reload: ${sig1} then ${c.sig}`;
    if (!/[1-9]/.test(sig1)) return `the picture is blank: ${sig1}`;
  });
  await step("loading Slot 1 after the reload restores the saved machine", async () => {
    const paused = await tickNow();
    await press("slot:1");
    await page.waitFor("the load", (s) => s.running && !s.menu_open && /Loaded Slot 1/.test(s.status), 10000);
    await sleep(300);
    const after = await tickNow();
    return after < savedTick + 80_000_000 ? true : `the machine is at ${after}; the save was taken near ${savedTick}`;
  });
  await step("Export hands the focused slot over as a file (where the sheet is wide enough to show it)", async () => {
    await openSheet("Load");
    const names = (await slots()).hits.map((h) => h[0]);
    if (!names.includes("file:export")) return true;
    await page.cdp.send("Page.setDownloadBehavior", { behavior: "deny" }, page.top).catch(() => {});
    await press("file:export");
    const u = await page.waitFor("the export toast", (s) => /Exported .*\.psxstate/.test(s.status), 8000);
    return u ? true : "no export";
  });
  await step("a save made on another disc or build is listed as such and refused, the game untouched", async () => {
    await mutateSave(page, gameId, 1, "disc");
    await page.goto(urlFor());
    await page.waitFor("the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
    await startAgain(page, size);
    await openSheet("Load");
    const c = (await slots()).cards[1];
    if (!/different disc/i.test(c.problem)) return `no disc problem on the card: ${JSON.stringify(c)}`;
    await page.shot(`${label}-7-other-disc`);
    await press("slot:1");
    await page.waitFor("the explanation", (s) => s.slots.ask === "problem:1", 3000);
    await page.shot(`${label}-8-other-disc-asks`);
    const tick = await tickNow();
    await press("ask:ok");
    await page.waitFor("the question gone", (s) => s.slots.ask === null, 3000);
    const u = await page.ux();
    if (u.running || !u.menu_open) return "a refused load changed the transport";
    if (u.cpu_tick !== tick) return "a refused load changed the machine";
  });
  await step("a save from a newer PSoXide is refused with its own message", async () => {
    await mutateSave(page, gameId, 1, "format");
    await page.goto(urlFor());
    await page.waitFor("the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
    await startAgain(page, size);
    await openSheet("Load");
    const c = (await slots()).cards[1];
    return /newer/i.test(c.problem) ? true : `card: ${JSON.stringify(c)}`;
  });
  await step("a damaged save says so and leaves the game running", async () => {
    // Put a good save in Slot 2, then cut it short.
    await openSheet("Save");
    await press("slot:2");
    await page.waitFor("the save", (s) => s.slots.cards[2].stamp > 0, 6000);
    await truncateSave(page, gameId, 2);
    await page.goto(urlFor());
    await page.waitFor("the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home", 90000);
    await startAgain(page, size);
    await openSheet("Load");
    const tick = await tickNow();
    await press("slot:2");
    const u = await page.waitFor("the refusal", (s) => /damaged/i.test(s.status), 8000);
    if (u.running) return "the damaged load resumed the game";
    if ((await tickNow()) !== tick) return "the machine changed";
    await page.shot(`${label}-9-damaged`);
  });
  await step("delete asks first and then empties the slot", async () => {
    await press("slot:2:delete");
    await page.waitFor("the question", (s) => s.slots.ask === "delete:2", 3000);
    await press("ask:cancel");
    if ((await slots()).cards[2].stamp === 0) return "Cancel deleted it";
    await press("slot:2:delete");
    await page.waitFor("the question", (s) => s.slots.ask === "delete:2", 3000);
    await press("ask:confirm");
    await page.waitFor("the slot empty", (s) => s.slots.cards[2].stamp === 0, 5000);
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

/** Touch preview and the phone's tab bar are centred, at this size. */
async function journeyCentre(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(SILENT);
  await page.goto(urlFor());
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
  await page.tapKey("x");
  await step("the Controls > Touch layout preview is centred in its column", async () => {
    await (mobile ? homeTap : homeClick)(page, "nav:Controls");
    await page.waitFor("Controls", (s) => s.home.page === "Controls", 4000);
    if (mobile) { await (mobile ? homeTap : homeClick)(page, "ctl:tab:Touch"); await page.waitFor("Touch", (s) => s.home.controls.tab === 2, 4000); }
    await sleep(900);
    for (let i = 0; i < 14 && !(await page.ux()).home.controls.preview; i++) {
      await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: size.w / 2, y: size.h / 2, deltaX: 0, deltaY: 200 }, page.top);
      await sleep(250);
    }
    await sleep(500);
    await page.shot(`${label}-preview`);
    const p = (await page.ux()).home.controls.preview;
    if (!p) return "the preview is not on screen";
    const [px0, px1, cx0, cx1] = p;
    const off = (px0 + px1) / 2 - (cx0 + cx1) / 2;
    return Math.abs(off) <= 1 ? true : `the preview is ${off.toFixed(1)} px off the centre of its column`;
  });
  if (mobile) await step("the tab bar is centred in the viewport", async () => {
    await homeTap(page, "nav:Home");
    await page.waitFor("Home", (s) => s.home.page === "Home", 4000);
    await sleep(600);
    await page.shot(`${label}-tabbar`);
    const u = await page.ux();
    const tabs = u.home.hits.filter((h) => /^nav:/.test(h[0]));
    const left = Math.min(...tabs.map((h) => h[1])), right = Math.max(...tabs.map((h) => h[3]));
    const off = (left + right) / 2 - size.w / 2;
    return Math.abs(off) <= 1.5 ? true : `the bar spans ${left.toFixed(1)}..${right.toFixed(1)} of ${size.w}`;
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
    const before = (await page.ux()).home;
    await hold({ 15: true }); // D-pad right: the next case
    const after = (await page.ux()).home;
    if (before.case === after.case) return `focus stayed on case ${before.case}`;
    await hold({ 1: true }); // B = Circle = back
    await page.waitFor("resumed", (s) => !s.menu_open && s.running, 5000);
  });
  await step("pad navigation: Cross on Game / Reset restarts the game in place", async () => {
    const before = await page.ux();
    await hold({ 8: true, 9: true }, null, 250);
    await page.waitFor("menu open", (s) => s.menu_open, 5000);
    await hold({ 5: true }); // R1: the next section, Game
    await page.waitFor("the Game section", (s) => s.home.page === "Game", 4000);
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
const RESET_FROM_RIGHT = 400;
// The brightest the whole screen may get during the boot flash (mean luma, 0 to 1). The old flat white wash read about 0.5.
const FLASH_LUMA_MAX = 0.3;
const PAUSE_FROM_RIGHT = 437;

/**
 * Walk the open menu with the keyboard to a column and a row (by label substring), reading
 * the selection back from the state probe. Enter on the row when `enter` is set.
 */
/** Escape until the shell is gone (a section goes back to Home first). */

/**
 * A slow or hidden browser pane gives the page a few animation frames a second. The launch sequence is
 * driven by the clock, not by the frame count, so it still takes its 4.6 s; any input skips it; and a
 * click, a tap, Enter or a quick pad press that lands between two frames is not dropped.
 */
async function journeyThrottled(label, urlFor, size, fps = 4) {
  current = label;
  const mobile = !!size.mobile;
  const open = async () => {
    const page = await newPage(label);
    await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
    await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
    await page.throttleFrames(fps);
    await page.goto(urlFor());
    return page;
  };
  const close = (page) => cdp.send("Target.closeTarget", { targetId: page.targetId });
  const activate = async (page, name) => (mobile ? homeTap(page, name) : homeClick(page, name));
  /** Watch a launch from the moment it is asked for until the game is running; returns the clock readings. */
  const watchLaunch = async (page, t0, limitMs = 40000) => {
    const series = [];
    while (Date.now() - t0 < limitMs) {
      const u = await page.ux();
      if (u) {
        series.push([Date.now() - t0, u.home?.launch ?? -1]);
        if (u.running && !u.menu_open) return { ms: Date.now() - t0, series };
      }
      await sleep(100);
    }
    return { ms: null, series };
  };
  const ready = (page) => page.waitFor("the home screen with the disc ready", (s) => s.menu_open && !s.running && s.loaded_id && s.home?.page === "Home" && s.home.countdown !== "running", 120000);

  let page = await open();
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 120000);
  await page.tapKey("x");
  await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 10000);
  await step(`at ${fps} frames a second, clicking the nav switches sections`, async () => {
    for (const section of ["Controls", "Settings", "Home"]) {
      await activate(page, `nav:${section}`);
      await page.waitFor(`the ${section} page`, (s) => s.home.page === section, 6000);
    }
  });
  await step(`at ${fps} frames a second, PageDown and Escape switch sections without dropping a press`, async () => {
    await page.tapKey("PageDown");
    await page.waitFor("the next section", (s) => s.home.page !== "Home", 6000);
    await page.tapKey("Escape");
    await page.waitFor("Home again", (s) => s.home.page === "Home", 6000);
  });
  await ready(page);
  await step(`at ${fps} frames a second, Enter starts the focused disc and the launch takes about 2.3 s`, async () => {
    const t0 = Date.now();
    await page.tapKey("Enter");
    const r = await watchLaunch(page, t0);
    await page.shot(`${label}-launch-done`);
    if (r.ms == null) return `the disc did not start in 40 s: ${JSON.stringify(r.series.slice(-6))}`;
    // The sequence clock tracks the wall clock, not the frame count.
    const seen = r.series.filter(([, t]) => t >= 1);
    if (!seen.length) return `the launch clock never moved: ${JSON.stringify(r.series.slice(0, 8))}`;
    const worst = Math.max(...seen.map(([ms, t]) => Math.abs(ms / 1000 - t)));
    if (r.ms < 1700 || r.ms > 4500) return `the launch took ${r.ms} ms: ${JSON.stringify(r.series.filter((_, i) => i % 6 === 0))}`;
    if (worst > 1.6) return `the launch clock drifted ${worst.toFixed(2)} s from the wall clock`;
  });
  await close(page);

  page = await open();
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 120000);
  await page.tapKey("x");
  await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 10000);
  await ready(page);
  await step(`at ${fps} frames a second, a click or tap on Start disc launches, and any input skips the sequence`, async () => {
    const t0 = Date.now();
    await activate(page, "primary");
    await page.waitFor("the launch under way", (s) => s.home.launch >= 0, 8000);
    await sleep(600);
    await page.tapKey("x");
    const r = await watchLaunch(page, t0);
    if (r.ms == null) return `the disc did not start: ${JSON.stringify(r.series.slice(-6))}`;
    if (r.ms > 2300) return `skipping still took ${r.ms} ms`;
  });
  await close(page);

  if (!mobile) {
    page = await open();
    await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 120000);
    await page.tapKey("x");
    await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 10000);
    await ready(page);
    await step(`at ${fps} frames a second, a quick pad press (Cross) on the focused Start disc launches it`, async () => {
      await page.padConnect();
      await page.waitFor("the pad", (s) => s.menu_open, 6000);
      await sleep(1200);
      const t0 = Date.now();
      await page.padSet({ 0: true });
      await sleep(70);
      await page.padSet({});
      const r = await watchLaunch(page, t0);
      if (r.ms == null) return "a 70 ms Cross press between two frames did not start the disc";
    });
    await close(page);
  }
}


/** The areas of the Controls page that must stay clear of each other and of the screen edges. */
function controlsLayoutProblem(u, size) {
  const [l, t, r, b] = insetsFor(size);
  const own = u.home.hits.filter(([n]) => /^(ctl:|slider:Touch|row:Touch|row:Left-handed)/.test(n));
  for (const [n, x0, y0, x1, y1] of own) {
    if (x0 < l - 1 || y0 < t - 1 || x1 > size.w - r + 1 || y1 > size.h - b + 1) return `${n} [${x0},${y0},${x1},${y1}] is outside the safe area`;
    // A control scrolled half out of the view is cut by it: judge the ones that are whole.
    const [vt, vb] = u.home.controls.view;
    const whole = y0 > vt + 1 && y1 < vb - 1;
    if (size.mobile && whole && (y1 - y0 < 43.5 || (x1 - x0 < 43.5 && !n.startsWith("slider")))) return `${n} is ${Math.round(x1 - x0)} x ${Math.round(y1 - y0)}, under the 44 px touch target`;
  }
  const core = (h) => [h[1] + 3, h[2] + 3, h[3] - 3, h[4] - 3];
  for (let i = 0; i < own.length; i++) {
    for (let j = i + 1; j < own.length; j++) {
      // A slider sits on top of its own row: a tap on the row's label still toggles or cycles it.
      if (own[i][0].replace(/^(row|slider):/, "") === own[j][0].replace(/^(row|slider):/, "")) continue;
      const [a0, b0, a1, b1] = core(own[i]), [c0, d0, c1, d1] = core(own[j]);
      if (Math.min(a1, c1) - Math.max(a0, c0) > 1 && Math.min(b1, d1) - Math.max(b0, d0) > 1) return `${own[i][0]} and ${own[j][0]} overlap`;
    }
  }
  return null;
}

/**
 * The Controls page: live device cards, the remappable keyboard map, the analog toggle and the touch
 * controller, inside the shell, reached by click, tap, keyboard and pad.
 */
async function journeyControls(label, urlFor, size) {
  current = label;
  const mobile = !!size.mobile;
  const page = await newPage(label);
  await page.viewport(size.w, size.h, { dpr: size.dpr, mobile });
  await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
  await page.goto(urlFor());
  /** Scroll the page with the wheel until a control is fully on screen. */
  const reveal = async (name) => {
    for (let i = 0; i < 12; i++) {
      const u = await page.ux();
      const h = u.home.hits.find((x) => x[0] === name);
      const view = u.home.controls.view;
      if (h && h[4] - h[2] >= 40 && h[2] > view[0] && h[4] < view[1]) return;
      // Not drawn at all: go back to the top first. Cut by the top edge: up a little. Otherwise down.
      const dy = !h && i === 0 ? -4000 : h && h[2] <= view[0] + 1 ? -120 : 160;
      await page.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: size.w / 2, y: size.h / 2, deltaX: 0, deltaY: dy }, page.top);
      await sleep(250);
    }
  };
  const activate = async (name) => {
    if (/^(ctl:(?!tab)|row:Touch|slider:)/.test(name)) await reveal(name);
    return mobile ? homeTap(page, name) : homeClick(page, name);
  };
  const ctl = async () => (await page.ux()).home.controls;
  const hasHit = async (name) => (await page.ux()).home.hits.some((h) => h[0] === name);
  /** The phone shows one group at a time. */
  const showGroup = async (name) => {
    if (await hasHit(`ctl:tab:${name}`)) {
      await activate(`ctl:tab:${name}`);
      await page.waitFor(`the ${name} group`, (s) => s.home.controls.tab === ["Devices", "Keyboard", "Touch"].indexOf(name), 4000);
      await sleep(250);
    }
  };
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 120000);
  await page.tapKey("x");
  await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 10000);
  await step("Controls opens inside the shell; each group fits the screen with 44 px targets and nothing overlaps", async () => {
    await activate("nav:Controls");
    await page.waitFor("the Controls page", (s) => s.home.page === "Controls" && s.panel === "controls", 4000);
    await sleep(500);
    for (const group of ["Devices", "Keyboard", "Touch"]) {
      await showGroup(group);
      await page.shot(`${label}-controls-${group.toLowerCase()}`);
      const names = (await page.ux()).home.hits.map((h) => h[0]);
      const need = { Devices: ["ctl:port:"], Keyboard: ["ctl:key:Cross", "ctl:key:L1", "ctl:reset"], Touch: ["slider:Touch opacity", "row:Touch haptics"] }[group];
      for (const n of need) {
        // Scrolling brings anything below the fold on a small screen; a prefix is a device id.
        if (!n.endsWith(":")) await reveal(n);
        const now = (await page.ux()).home.hits.map((h) => h[0]);
        if (!now.some((x) => x.startsWith(n))) return `${group}: no ${n} in ${now.filter((x) => x.startsWith("ctl") || x.startsWith("slider") || x.startsWith("row"))}`;
      }
      const problem = controlsLayoutProblem(await page.ux(), size);
      if (problem) return `${group}: ${problem}`;
    }
    const names = (await page.ux()).home.hits.map((h) => h[0]);
    if (mobile && !names.includes("ctl:tab:Keyboard")) return "no segmented bar on a phone";
    if (!mobile && size.w >= 900 && !names.some((x) => x.startsWith("ctl:key:"))) return "a wide window should show all three groups";
  });
  await step("a keyboard has no analog toggle; a controller does, and it starts analog and changes back", async () => {
    await showGroup("Devices");
    const c0 = await ctl();
    const kb = c0.devices.find((d) => /key/i.test(d[0]));
    if (!kb) return `no keyboard card: ${JSON.stringify(c0.devices)}`;
    const names = (await page.ux()).home.hits.map((h) => h[0]);
    if (names.includes(`ctl:mode:${kb[0]}`)) return "the keyboard card offers an analog toggle";
    if (/analog/i.test(String(kb[2]))) return `the keyboard is ${kb[2]}`;
    await page.padConnect();
    await sleep(400);
    await page.padSet({ 0: true });
    await sleep(150);
    await page.padSet({});
    const c1 = (await page.waitFor("a controller card", (s) => s.home.controls.devices.some((d) => !/key|touch/i.test(d[0])), 6000)).home.controls;
    const dev = c1.devices.find((d) => !/key|touch/i.test(d[0]));
    if (!/analog/i.test(String(dev[2]))) return `a controller with sticks does not start analog: ${JSON.stringify(dev)}`;
    await activate(`ctl:mode:${dev[0]}`);
    const c2 = (await page.waitFor("the profile changed", (s) => s.home.controls.devices.find((d) => d[0] === dev[0])?.[2] !== dev[2], 4000)).home.controls;
    await activate(`ctl:mode:${dev[0]}`);
    await page.waitFor("the profile toggled again", (s) => s.home.controls.devices.find((d) => d[0] === dev[0])?.[2] !== c2.devices.find((d) => d[0] === dev[0])?.[2], 4000);
    await activate(`ctl:port:${dev[0]}`);
    await page.waitFor("the port changed", (s) => s.home.controls.devices.find((d) => d[0] === dev[0])?.[1] !== dev[1], 4000);
  });
  await step("a key is rebound by selecting it and pressing the new one; Escape cancels; Reset restores", async () => {
    await showGroup("Keyboard");
    const before = (await ctl()).keys.Cross;
    await activate("ctl:key:Cross");
    await page.waitFor("a key being bound", (s) => s.home.controls.capture === "Cross", 4000);
    await page.shot(`${label}-controls-binding`);
    await page.tapKey("Escape");
    await page.waitFor("the binding cancelled", (s) => s.home.controls.capture === null, 4000);
    if ((await page.ux()).home.page !== "Controls") return "Escape left the page while a key was being bound";
    await activate("ctl:key:Cross");
    await page.waitFor("a key being bound", (s) => s.home.controls.capture === "Cross", 4000);
    await page.tapKey("z");
    const after = (await page.waitFor("the key bound", (s) => s.home.controls.capture === null && s.home.controls.keys.Cross !== before, 4000)).home.controls.keys.Cross;
    if (!/^z$/i.test(after)) return `Cross is ${after}, not Z`;
    await activate("ctl:reset");
    await page.waitFor("the map reset", (s) => s.home.controls.keys.Cross === before, 4000);
  });
  await step("binding a key another action holds swaps the two and says so; nothing is left unbound", async () => {
    await showGroup("Keyboard");
    const k0 = (await ctl()).keys;
    const cross = k0.Cross, triangle = k0.Triangle;
    if (!cross || !triangle || cross === triangle) return `unexpected defaults: ${JSON.stringify(k0)}`;
    await activate("ctl:key:Cross");
    await page.waitFor("a key being bound", (s) => s.home.controls.capture === "Cross", 4000);
    await page.tapKey(triangle.length === 1 ? triangle.toLowerCase() : triangle);
    const s = await page.waitFor("the key bound", (x) => x.home.controls.capture === null && x.home.controls.keys.Cross !== cross, 4000);
    await page.shot(`${label}-controls-swapped`);
    const k1 = s.home.controls.keys;
    if (k1.Cross.toLowerCase() !== triangle.toLowerCase()) return `Cross is ${k1.Cross}, not ${triangle}`;
    if (k1.Triangle.toLowerCase() !== cross.toLowerCase()) return `Triangle is ${k1.Triangle}, not Cross's old ${cross}: ${JSON.stringify(k1)}`;
    const none = Object.entries(k1).filter(([, v]) => /^(none|-)$/i.test(v)).map(([t]) => t);
    const before0 = Object.entries(k0).filter(([, v]) => /^(none|-)$/i.test(v)).map(([t]) => t);
    if (none.length !== before0.length) return `an action was left unbound: ${none}`;
    if (!/moved to/.test(s.status)) return `the toast does not say what moved: "${s.status}"`;
    await activate("ctl:reset");
    await page.waitFor("the map reset", (x) => x.home.controls.keys.Cross === cross, 4000);
  });
  if (!mobile) await step("the keyboard moves between rows and along a row, Enter starts a binding, Escape cancels, then goes Home", async () => {
    await showGroup("Keyboard");
    let focus = (await ctl()).focus;
    let moved = 0;
    const path = [focus];
    // The last step left the focus on Reset, below the keys; the narrow layout starts from the top.
    const toward = focus === "ctl:reset" || focus.startsWith("ctl:touch") ? "ArrowUp" : "ArrowDown";
    for (let i = 0; i < 14 && !focus.startsWith("ctl:key:"); i++) {
      await page.tapKey(toward);
      await sleep(160);
      const next = (await ctl()).focus;
      if (next !== focus) moved++;
      focus = next;
      path.push(focus);
    }
    if (!focus.startsWith("ctl:key:")) return `Down never reached the keys: ${path.join(" > ")}`;
    await page.tapKey("ArrowRight");
    await sleep(200);
    const right = (await ctl()).focus;
    if (right === focus) return `Right did not move along the row from ${focus}`;
    await page.tapKey("Enter");
    const armed = (await page.waitFor("a key being bound", (s) => s.home.controls.capture !== null, 4000)).home.controls.capture;
    await page.tapKey("Escape");
    await page.waitFor("the binding cancelled", (s) => s.home.controls.capture === null, 4000);
    await page.tapKey("Escape");
    await page.waitFor("Home", (s) => s.home.page === "Home", 4000);
    return armed ? true : "no key was armed";
  });
  await step("the touch rows work: a toggle flips and a slider drags", async () => {
    if ((await page.ux()).home.page !== "Controls") { await activate("nav:Controls"); await page.waitFor("Controls", (s) => s.home.page === "Controls", 4000); }
    await showGroup("Touch");
    const v0 = (await ctl()).touch;
    await activate("row:Touch haptics");
    await page.waitFor("haptics flipped", (s) => s.home.controls.touch["Touch haptics"] !== v0["Touch haptics"], 4000);
    await activate("row:Touch haptics");
    await page.waitFor("haptics back", (s) => s.home.controls.touch["Touch haptics"] === v0["Touch haptics"], 4000);
    const [x0, y0, x1, y1] = (await page.ux()).home.hits.find((h) => h[0] === "slider:Touch opacity").slice(1);
    const y = (y0 + y1) / 2;
    await page.mouse("mouseMoved", x0 + 20, y);
    await page.mouse("mousePressed", x0 + 20, y);
    for (let i = 1; i <= 6; i++) { await sleep(100); await page.mouse("mouseMoved", x0 + 20 + i * ((x1 - x0) * 0.6 / 6), y); }
    await page.mouse("mouseReleased", x0 + 20 + (x1 - x0) * 0.6, y);
    await page.waitFor("opacity changed", (s) => s.home.controls.touch["Touch opacity"] !== v0["Touch opacity"], 4000);
    await page.shot(`${label}-controls-touch-after`);
  });
  await step("page logged no errors", () => (page.logs.length === 0 ? true : page.logs.slice(0, 3).join(" | ")));
  await cdp.send("Target.closeTarget", { targetId: page.targetId });
}

async function closeShell(page) {
  for (let i = 0; i < 4 && (await page.ux()).menu_open; i++) { await page.tapKey("Escape"); await sleep(250); }
  await page.waitFor("the shell closed", (x) => !x.menu_open, 4000);
}

async function menuSelect(page, column, row, { enter = true } = {}) {
  // The shell's sections are one ring (L1/R1, PageUp/PageDown): Home, Game (while a game is started),
  // Controls, Settings. Rows live on the Game and Settings pages.
  const PAGE = { Game: "Game", Settings: "Settings" };
  let dir = "ArrowDown", lastRow = null;
  for (let i = 0; i < 80; i++) {
    const u = await page.ux();
    const h = u.home;
    const onList = h.zone === "body" && h.page === PAGE[column] && u.menu.column === column;
    if (onList) {
      if (!u.menu.row.includes(row)) {
        // The lists do not wrap: turn around at the end.
        if (u.menu.row === lastRow) dir = dir === "ArrowDown" ? "ArrowUp" : "ArrowDown";
        lastRow = u.menu.row;
        await page.tapKey(dir);
        continue;
      }
      if (enter) await page.tapKey("Enter");
      return;
    }
    if (h.zone === "nav") await page.tapKey("ArrowDown");
    else await page.tapKey("PageDown");
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
    chrome.kill("SIGKILL");
    startedChromes.delete(chrome);
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
  await menuSelect(page, "Settings", "Video scale", { enter: false });
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
  // A build with the home screen waits there with the disc ready: press Start disc.
  const first = await page.waitFor("the player or the home screen", (s) => s.running || (s.home && s.menu_open && s.loaded_id), 120000).catch(() => null);
  // The first press may only cancel a first-visit countdown: press until the disc runs.
  for (let i = 0; first && !first.running && first.home && i < 4 && !(await page.ux())?.running; i++) {
    await page.tapKey("Enter");
    await sleep(700);
  }
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

async function scenarioTap() {
  const size = { w: 1280, h: 800, dpr: 1, mobile: false };
  const page = await newPage("tap");
  await page.viewport(size.w, size.h, { dpr: 1 });
  await page.goto(direct());
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 90000);
  await page.tapKey("x");
  await homeClick(page, "nav:Settings");
  await sleep(500);
  await menuSelect(page, "Settings", "Mute", { enter: false });
  await page.tapKey("ArrowRight"); await sleep(300); await page.tapKey("ArrowRight");
  await menuSelect(page, "Settings", "Volume", { enter: false });
  const u = await page.ux();
  const v = u.home.hits.find((h) => h[0] === "slider:Volume");
  console.log("slider", JSON.stringify(u.home.hits.slice(0, 16)), await page.eval("innerWidth"));
  const [x0, y0, x1, y1] = v.slice(1);
  const y = (y0 + y1) / 2;
  await page.mouse("mouseMoved", x0 + 20, y);
  await page.mouse("mousePressed", x0 + 20, y);
  for (let i = 0; i < 6; i++) { await sleep(100); await page.mouse("mouseMoved", x0 + 20 + i * 15, y); console.log(i, JSON.stringify((await page.ux()).menu)); }
  await page.mouse("mouseReleased", x0 + 110, y);
  await sleep(300);
  await menuSelect(page, "Settings", "Volume", { enter: false });
  console.log("end", JSON.stringify((await page.ux()).menu));
}

/** Screenshots at four sizes (works against any build): the first screen, playing, and the menu. */
async function scenarioShots() {
  const SIZES = [["desktop", 1280, 800, 1, false], ["phone", 393, 852, 3, true], ["small-phone", 360, 640, 3, true], ["phone-landscape", 852, 393, 3, true]];
  for (const [name, w, h, dpr, mobile] of SIZES) {
    await inChrome(dpr, async () => {
      const page = await newPage(name);
      await page.viewport(w, h, { dpr, mobile });
      await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
      await page.goto(direct());
      await page.waitFor("the player", (s) => s.has_bus, 90000);
      await sleep(2500);
      await page.shot(`${args.label}-${name}-1-open`);
      await waitDemoRunning(page, 120000);
      await sleep(2500);
      await page.shot(`${args.label}-${name}-2-playing`);
      if (mobile) {
        const t = (await page.ux()).touch;
        const m = t?.ui.find((b) => b[0] === "menu");
        if (m) await page.tap(m[1] / dpr, m[2] / dpr);
      } else {
        await page.tapKey("Escape");
      }
      await sleep(1500);
      await page.shot(`${args.label}-${name}-3-menu`);
      await cdp.send("Target.closeTarget", { targetId: page.targetId });
    });
  }
}

/**
 * Review screenshots at the five sizes the design is checked at (`--scenario review --label before|after
 * --shots <dir>`): the home screen, the Controls cards, the launch hand-off, the pause shell over a game
 * (Game and Settings pages), and the phone menu button reached with a mouse click. Works against any build.
 */
async function scenarioReview() {
  const SIZES = [["1280x800", 1280, 800, 1, false], ["800x600", 800, 600, 1, false], ["393x852", 393, 852, 3, true],
    ["360x640", 360, 640, 3, true], ["852x393", 852, 393, 3, true]];
  const only = args.size;
  for (const [name, w, h, dpr, mobile] of SIZES) {
    if (only && only !== name) continue;
    await inChrome(dpr, async () => {
      const tag = `${args.label ?? "review"}-${name}`;
      const page = await newPage(name);
      await page.viewport(w, h, { dpr, mobile });
      await page.addInit(`try { localStorage.setItem("psoxide.silenthint", "1"); } catch (e) {}`);
      await page.goto(direct("?countdown=30"));
      await page.waitFor("the countdown", (s) => s.home?.countdown === "running" && !s.running, 120000);
      await page.tapKey("x");
      await page.waitFor("a cancelled countdown", (s) => s.home.countdown === "cancelled", 10000);
      await sleep(600);
      const activate = async (n) => (mobile ? homeTap(page, n) : homeClick(page, n));
      await page.shot(`${tag}-2-home`);
      await activate("nav:Controls");
      await page.waitFor("Controls", (s) => s.home.page === "Controls", 4000);
      await sleep(900);
      await page.shot(`${tag}-6-controls-cards`);
      await activate("nav:Home");
      await page.waitFor("Home", (s) => s.home.page === "Home", 4000);
      await sleep(400);
      await activate("primary");
      const t0 = Date.now();
      const marks = [1.6, 1.9, 2.1, 2.3, 2.5, 2.8, 3.2];
      let next = 0;
      while (Date.now() - t0 < 40000) {
        const secs = (Date.now() - t0) / 1000;
        if (next < marks.length && secs >= marks[next]) { await page.shot(`${tag}-5-launch-${marks[next]}s`); next++; }
        if (next >= marks.length) break;
        await sleep(60);
      }
      await waitDemoRunning(page, 90000);
      await sleep(2500);
      await page.shot(`${tag}-playing`);
      if (mobile) {
        const m = (await page.ux()).touch?.ui.find((b) => b[0] === "menu");
        if (m) await page.click(m[1] / dpr, m[2] / dpr);
        await sleep(900);
        const viaMouse = (await page.ux()).menu_open;
        console.log(`${tag}: menu button by mouse click opened the shell: ${viaMouse}`);
        await page.shot(`${tag}-3-menu-by-mouse`);
        if (!viaMouse && m) await page.tap(m[1] / dpr, m[2] / dpr);
      } else {
        await page.tapKey("Escape");
      }
      await page.waitFor("the shell", (s) => s.menu_open && s.home.page === "Home", 8000);
      await sleep(700);
      await page.shot(`${tag}-7-shell-home`);
      for (const [section, n] of [["Game", "1-game"], ["Settings", "1-settings"]]) {
        await activate(`nav:${section}`);
        await page.waitFor(`the ${section} page`, (s) => s.home.page === section, 4000);
        await sleep(800);
        await page.shot(`${tag}-${n}`);
      }
      await cdp.send("Target.closeTarget", { targetId: page.targetId });
    });
  }
}

/** The launch sequence's cost: frames per second and the worst frame while it plays (use --throttle for a phone). */
async function scenarioLaunch() {
  const w = Number(args.width ?? 1280), h = Number(args.height ?? 800);
  const page = await newPage("launch");
  await page.viewport(w, h, { dpr: 1 });
  if (args.throttle) await page.throttle(Number(args.throttle));
  await page.goto(direct("?countdown=30"));
  await page.waitFor("the countdown", (s) => s.home?.countdown === "running", 120000);
  await page.tapKey("x");
  await sleep(1500);
  await page.eval(`(async () => { const m = await import(window.psoxideWasm.js); m.psoxideBenchReset(); })()`);
  const t0 = Date.now();
  await page.tapKey("Enter");
  // Only the sequence itself: up to the moment it hands over to the boot.
  while (Date.now() - t0 < 30000) {
    const u = await page.ux();
    if ((u.running && !u.menu_open) || u.home?.launch >= 2.1) break;
    await sleep(100);
  }
  const st = await page.bench();
  const secs = (Date.now() - t0) / 1000;
  console.log(JSON.stringify({ throttle: Number(args.throttle ?? 1), size: `${w}x${h}`, seconds: +secs.toFixed(1), redraws_per_s: +(st.redraws / secs).toFixed(1), ms_per_redraw: +(st.total_ms / Math.max(1, st.redraws)).toFixed(2), egui_ms_per_redraw: +(st.egui_ms / Math.max(1, st.redraws)).toFixed(2), max_redraw_ms: +st.max_total_ms.toFixed(1), long_redraws: st.long_redraws }));
}

/** One live check, no stubs: a served page fetches a real xlenore cover by a real serial, across origins. */
async function scenarioCors() {
  const page = await newPage("cors");
  await page.goto(direct());
  await sleep(1500);
  const serial = args.serial ?? "SCUS-94426";
  const out = await page.eval(`(async () => {
    const r = await fetch("https://raw.githubusercontent.com/xlenore/psx-covers/main/covers/default/${serial}.jpg", { mode: "cors" });
    const bytes = new Uint8Array(await r.arrayBuffer());
    return JSON.stringify({ origin: location.origin, status: r.status, type: r.headers.get("content-type"), bytes: bytes.length, jpeg: bytes[0] === 0xff && bytes[1] === 0xd8 });
  })()`, page.top);
  console.log("xlenore live:", out);
}

async function main() {
  if (args.scenario === "launch") { await inChrome(Number(args.hostdpr ?? 1), scenarioLaunch); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "fps") { await inChrome(1, async () => {
    const page = await newPage("fps");
    await page.viewport(1280, 800, { dpr: 1 });
    if (args.fps) await page.throttleFrames(Number(args.fps));
    await page.goto(direct("?countdown=30"));
    await page.waitFor("the countdown", (s) => s.home?.countdown === "running", 120000);
    const a = await page.bench(); await sleep(3000); const b = await page.bench();
    console.log(JSON.stringify({ throttle: args.fps ?? null, redraws_per_s: (b.redraws - a.redraws) / 3 }));
  }); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "toolbar") { await inChrome(1, async () => {
    const page = await newPage("toolbar");
    await page.viewport(1280, 800, { dpr: 1 });
    await page.addInit(SILENT);
    await page.goto(direct("?countdown=12"));
    await bootDemo(page, { mobile: false });
    console.log("toolbar icons at 1280:", JSON.stringify((await toolbarIcons(page)).map((v) => Math.round(v))));
    await page.viewport(800, 600, { dpr: 1 });
    await sleep(800);
    console.log("toolbar icons at 800:", JSON.stringify((await toolbarIcons(page)).map((v) => Math.round(v))));
  }); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "cors") { await inChrome(1, scenarioCors); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "review") { await scenarioReview(); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "shots") { await scenarioShots(); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "tap") { await inChrome(1, scenarioTap); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "bench") { await inChrome(1, scenarioBench); siteServer?.close(); parentServer?.close(); process.exit(0); }
  if (args.scenario === "spam") { await inChrome(1, scenarioSpam); siteServer?.close(); parentServer?.close(); process.exit(0); }
  const wanted = (name) => !ONLY || name.includes(ONLY);
  try {
    if (!ONLY || ONLY.includes("desktop") || ["gamepad", "menus", "nowebgpu", "home", "covers", "shell", "throttled", "controls", "grouping", "selection", "cover-states", "hints", "polish", "debug", "slots", "centre"].some((k) => ONLY.includes(k))) {
      await inChrome(1, async () => {
        if (wanted("desktop-direct")) await journeyDesktop("desktop-direct", () => direct());
        if (wanted("desktop-home")) await journeyHome("desktop-home", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-shell")) await journeyShell("desktop-shell", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-controls")) await journeyControls("desktop-controls", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-controls-narrow")) await journeyControls("desktop-controls-narrow", () => direct("?countdown=12"), { w: 800, h: 700, dpr: 1, mobile: false });
        if (wanted("desktop-throttled")) await journeyThrottled("desktop-throttled", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-covers")) await journeyCovers("desktop-covers", () => direct());
        if (wanted("desktop-grouping")) await journeyGrouping("desktop-grouping", () => direct(), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-cover-states")) await journeyCoverStates("desktop-cover-states", () => direct(), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-selection")) await journeySelection("desktop-selection", () => direct(), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-hints")) await journeyHints("desktop-hints", () => direct(), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-polish")) await journeyPolish("desktop-polish", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-polish-small")) await journeyPolish("desktop-polish-small", () => direct("?countdown=12"), { w: 800, h: 600, dpr: 1, mobile: false });
        if (wanted("desktop-debug")) await journeyDebug("desktop-debug", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-debug-small")) await journeyDebug("desktop-debug-small", () => direct("?countdown=12"), { w: 800, h: 600, dpr: 1, mobile: false });
        if (wanted("desktop-slots")) await journeySlots("desktop-slots", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-slots-small")) await journeySlots("desktop-slots-small", () => direct("?countdown=12"), { w: 800, h: 600, dpr: 1, mobile: false });
        if (wanted("desktop-centre")) await journeyCentre("desktop-centre", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-iframe")) await journeyDesktop("desktop-iframe", () => embedded(), { embeddedMode: true });
        if (wanted("desktop-list")) await journeyList("desktop-list", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-list-small")) await journeyList("desktop-list-small", () => direct("?countdown=12"), { w: 800, h: 600, dpr: 1, mobile: false });
        if (wanted("desktop-momentum")) await journeyMomentum("desktop-momentum", () => direct("?countdown=12"), { w: 1280, h: 800, dpr: 1, mobile: false });
        if (wanted("desktop-menus")) await journeyMenus("desktop-menus", () => direct());
        if (wanted("desktop-nowebgpu")) await journeyNoWebgpu("desktop-nowebgpu", () => direct());
        if (wanted("desktop-gamepad")) await journeyGamepad("desktop-gamepad", () => direct());
        if (wanted("desktop-gamepad-iframe")) await journeyGamepad("desktop-gamepad-iframe", () => embedded());
      });
    }
    if (!ONLY || ONLY.includes("phone") || ONLY.includes("home") || ONLY.includes("shell") || ONLY.includes("throttled") || ONLY.includes("controls") || ONLY.includes("grouping") || ONLY.includes("hints") || ONLY.includes("selection") || ONLY.includes("cover-states") || ONLY.includes("polish") || ONLY.includes("debug") || ONLY.includes("slots") || ONLY.includes("centre")) {
      await inChrome(3, async () => {
        if (wanted("phone-portrait")) await journeyPhone("phone-portrait", () => direct(), { w: 393, h: 852, dpr: 3 });
        if (wanted("phone-iframe")) await journeyPhone("phone-iframe", () => embedded(), { w: 393, h: 852, dpr: 3 });
        for (const [name, w, h] of [["phone-list", 393, 852], ["phone-list-375", 375, 667], ["phone-list-small", 360, 640], ["phone-list-landscape", 852, 393]]) {
          if (wanted(name)) await journeyList(name, () => direct("?countdown=12"), { w, h, dpr: 3, mobile: true });
        }
        for (const [name, w, h] of [["phone-momentum", 393, 852], ["phone-momentum-375", 375, 667], ["phone-momentum-small", 360, 640], ["phone-momentum-landscape", 852, 393]]) {
          if (wanted(name)) await journeyMomentum(name, () => direct("?countdown=12"), { w, h, dpr: 3, mobile: true });
        }
        if (wanted("phone-home")) await journeyHome("phone-home", () => direct("?countdown=12"), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-home-small")) await journeyHome("phone-home-small", () => direct("?countdown=12"), { w: 360, h: 640, dpr: 3, mobile: true });
        if (wanted("phone-home-landscape")) await journeyHome("phone-home-landscape", () => direct("?countdown=12"), { w: 852, h: 393, dpr: 3, mobile: true });
        if (wanted("phone-grouping")) await journeyGrouping("phone-grouping", () => direct(), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-cover-states")) await journeyCoverStates("phone-cover-states", () => direct(), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-selection")) await journeySelection("phone-selection", () => direct(), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-hints")) await journeyHints("phone-hints", () => direct(), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-polish")) await journeyPolish("phone-polish", () => direct("?countdown=12"), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-polish-375")) await journeyPolish("phone-polish-375", () => direct("?countdown=12"), { w: 375, h: 812, dpr: 3, mobile: true });
        if (wanted("phone-polish-small")) await journeyPolish("phone-polish-small", () => direct("?countdown=12"), { w: 360, h: 640, dpr: 3, mobile: true });
        if (wanted("phone-polish-landscape")) await journeyPolish("phone-polish-landscape", () => direct("?countdown=12"), { w: 852, h: 393, dpr: 3, mobile: true });
        if (wanted("phone-polish-iframe")) await journeyPolish("phone-polish-iframe", () => embedded("?countdown=12"), { w: 393, h: 852, dpr: 3, mobile: true, framed: true });
        for (const [name, size, url] of [
          ["phone-portrait", { w: 393, h: 852, dpr: 3 }, direct],
          ["phone-375", { w: 375, h: 812, dpr: 3 }, direct],
          ["phone-small", { w: 360, h: 640, dpr: 3 }, direct],
          ["phone-se", { w: 375, h: 667, dpr: 3 }, direct],
          ["phone-se-landscape", { w: 667, h: 375, dpr: 3 }, direct],
          ["phone-landscape", { w: 852, h: 393, dpr: 3 }, direct],
          ["phone-iframe", { w: 393, h: 852, dpr: 3 }, embedded],
        ]) {
          const q = () => url("?countdown=12");
          if (wanted(`${name}-debug`)) await journeyDebug(`${name}-debug`, q, { ...size, mobile: true });
          if (wanted(`${name}-slots`)) await journeySlots(`${name}-slots`, q, { ...size, mobile: true });
          if (wanted(`${name}-centre`)) await journeyCentre(`${name}-centre`, q, { ...size, mobile: true, framed: name === "phone-iframe" });
        }
        if (wanted("phone-shell")) await journeyShell("phone-shell", () => direct("?countdown=12"), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-shell-small")) await journeyShell("phone-shell-small", () => direct("?countdown=12"), { w: 360, h: 640, dpr: 3, mobile: true });
        if (wanted("phone-controls")) await journeyControls("phone-controls", () => direct("?countdown=12"), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-controls-small")) await journeyControls("phone-controls-small", () => direct("?countdown=12"), { w: 360, h: 640, dpr: 3, mobile: true });
        if (wanted("phone-controls-landscape")) await journeyControls("phone-controls-landscape", () => direct("?countdown=12"), { w: 852, h: 393, dpr: 3, mobile: true });
        if (wanted("phone-throttled")) await journeyThrottled("phone-throttled", () => direct("?countdown=12"), { w: 393, h: 852, dpr: 3, mobile: true });
        if (wanted("phone-shell-landscape")) await journeyShell("phone-shell-landscape", () => direct("?countdown=12"), { w: 852, h: 393, dpr: 3, mobile: true });
        if (wanted("phone-landscape")) await journeyPhone("phone-landscape", () => direct(), { w: 852, h: 393, dpr: 3 });
        if (wanted("phone-fullscreen")) await journeyFullscreenFallback("phone-fullscreen", () => direct(), { framed: false });
        if (wanted("phone-fullscreen-iframe")) await journeyFullscreenFallback("phone-fullscreen-iframe", () => embedded(), { framed: true });
        if (wanted("phone-small")) await journeyPhone("phone-small", () => direct(), { w: 360, h: 640, dpr: 3 });
        // The iPhone SE's screen, in both orientations (at the 3x the harness runs at): the shortest the controller is laid out for.
        if (wanted("phone-se")) await journeyPhone("phone-se", () => direct(), { w: 375, h: 667, dpr: 3 });
        if (wanted("phone-se-landscape")) await journeyPhone("phone-se-landscape", () => direct(), { w: 667, h: 375, dpr: 3 });
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
