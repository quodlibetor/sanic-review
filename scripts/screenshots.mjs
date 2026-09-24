// Screenshots of the demo dashboard for the README: builds and starts the
// `demo` example of sanic-web, drives headless Chrome over the DevTools
// protocol, and writes docs/images/*.png. `mise run screenshots` runs it.
//
// Chrome is `$CHROME`, else `google-chrome` on PATH. On Linux it gets a
// pinned Noto Color Emoji, downloaded once into ~/.cache/sanic-review/fonts.
// PNGs go through oxipng when it's installed.

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as sleep } from "node:timers/promises";

const OUT = "docs/images";
const SHOWCASE = "/pr/quodlibetor/frobnicator/42";

// Noto Color Emoji v2.051 (OFL), by commit, so a new release can't change
// the shots or what's run here unnoticed.
const EMOJI = {
  url: "https://raw.githubusercontent.com/googlefonts/noto-emoji/8998f5dd683424a73e2314a8c1f1e359c19e8742/fonts/NotoColorEmoji.ttf",
  file: "NotoColorEmoji-v2.051.ttf",
  sha256: "72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b",
};

// In order: opening a PR's page marks its review seen, so the index goes
// first.
// `prepare` runs in the page first; `until`, if it finds an element, is
// where the shot stops, else it's the whole page.
const openQuiet = `document.querySelector("#owed-quiet").open = true`;
const SHOTS = [
  { file: "index.png", path: "/", width: 1280, prepare: openQuiet },
  { file: "index-dark.png", path: "/", width: 1280, dark: true, prepare: openQuiet },
  // The summary and the drafts on the first file, the last of them
  // accepted; the second file's draft is next.
  { file: "drafts.png", path: SHOWCASE, width: 1100, until: `document.querySelectorAll("article.draft")[4]` },
  // The first file, with its drafts and thread.
  { file: "files.png", path: `${SHOWCASE}?view=files`, width: 1400, until: `document.querySelectorAll("div.file")[1]` },
];

// Run last first, each whether or not the one before it failed. Whatever
// starts a process or makes a dir adds its cleanup as soon as it has.
const cleanups = [];

async function main() {
  const fonts = await emojiFonts();
  const demo = await startDemo();
  const port = await startChrome(fonts);
  for (const shot of SHOTS) {
    const png = await capture(port, new URL(shot.path, demo).href, shot);
    const file = join(OUT, shot.file);
    writeFileSync(file, png);
    console.log(`wrote ${file}`);
  }
  if (spawnSync("oxipng", ["--version"]).status === 0) {
    run("oxipng", ["-o", "4", "--strip", "safe", ...SHOTS.map((s) => join(OUT, s.file))]);
  }
}

// Chrome on Linux falls back to fonts through fontconfig, and without one
// that has emoji the dashboard's 💬 and 👍 come out as empty boxes. Returns
// the directory of the pinned emoji font, fetching it on first use; `null`
// elsewhere, where the system's emoji font does.
async function emojiFonts() {
  if (process.platform !== "linux") return null;
  const dir = join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "sanic-review", "fonts");
  const file = join(dir, EMOJI.file);
  const check = (bytes, from) => {
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    if (sha256 !== EMOJI.sha256) {
      throw new Error(`${from} has sha256 ${sha256}, not the pinned ${EMOJI.sha256}`);
    }
  };
  if (existsSync(file)) {
    check(readFileSync(file), file);
    return dir;
  }
  console.log(`fetching ${EMOJI.url}`);
  const response = await fetch(EMOJI.url);
  if (!response.ok) throw new Error(`fetching ${EMOJI.url}: ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  check(bytes, EMOJI.url);
  mkdirSync(dir, { recursive: true });
  // Renamed into place, so an interrupted write never looks cached.
  writeFileSync(`${file}.part`, bytes);
  renameSync(`${file}.part`, file);
  return dir;
}

// Builds the demo and starts it; resolves with its URL.
async function startDemo() {
  const build = run("cargo", [
    "build", "--locked", "-p", "sanic-web", "--example", "demo", "--message-format=json-render-diagnostics",
  ]);
  const exe = build
    .split("\n")
    .filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line))
    .find((msg) => msg.reason === "compiler-artifact" && msg.target?.name === "demo" && msg.executable)?.executable;
  if (!exe) throw new Error("cargo built no demo executable");
  const child = spawn(exe, [], { stdio: ["ignore", "pipe", "inherit"] });
  cleanups.push(() => stop(child));
  return new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) => reject(new Error(`the demo exited with ${code}`)));
    createInterface({ input: child.stdout }).once("line", resolve);
  });
}

// Starts headless Chrome with a fresh profile, and with the fonts in
// `fonts` too if it's set; resolves with its DevTools port.
async function startChrome(fonts) {
  const tmp = mkdtempSync(join(tmpdir(), "sanic-screenshots-"));
  // Added first, so it runs once Chrome has stopped writing to it.
  cleanups.push(() => rmSync(tmp, { recursive: true, force: true }));
  const profile = join(tmp, "profile");
  const env = { ...process.env };
  if (fonts) {
    // The system's fonts, plus `fonts`, with fontconfig's cache kept here.
    env.FONTCONFIG_FILE = join(tmp, "fonts.conf");
    writeFileSync(
      env.FONTCONFIG_FILE,
      `<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
  <dir>${fonts}</dir>
  <cachedir>${join(tmp, "fontconfig")}</cachedir>
</fontconfig>
`,
    );
  }
  const child = spawn(
    process.env.CHROME || "google-chrome",
    [
      "--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`,
      "--no-first-run", "--no-default-browser-check", "--hide-scrollbars", "--disable-gpu",
      "about:blank",
    ],
    { stdio: "ignore", env },
  );
  cleanups.push(() => stop(child));
  // Not found, or gone before it's listening.
  let gone = null;
  child.once("error", (err) => (gone ??= err));
  child.once("exit", (code) => (gone ??= new Error(`Chrome exited with ${code}`)));
  const portFile = join(profile, "DevToolsActivePort");
  for (let i = 0; i < 100; i++) {
    if (gone) throw gone;
    // Read until it has a port, since Chrome may not have written it yet.
    const port = existsSync(portFile) && readFileSync(portFile, "utf8").split("\n")[0];
    if (port) return port;
    await sleep(100);
  }
  throw new Error("Chrome didn't start");
}

// A PNG of `url` at `width`; see SHOTS.
async function capture(port, url, { width, dark, prepare, until }) {
  const target = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: "PUT" })).json();
  const page = await connect(target.webSocketDebuggerUrl);
  try {
    await page.send("Page.enable");
    await page.send("Emulation.setDeviceMetricsOverride", { width, height: 900, deviceScaleFactor: 1, mobile: false });
    await page.send("Emulation.setEmulatedMedia", {
      features: [{ name: "prefers-color-scheme", value: dark ? "dark" : "light" }],
    });
    // The page writes some times in the browser's zone and locale.
    await page.send("Emulation.setTimezoneOverride", { timezoneId: "UTC" });
    await page.send("Emulation.setLocaleOverride", { locale: "en-US" });
    const loaded = page.once("Page.loadEventFired");
    const { errorText } = await page.send("Page.navigate", { url });
    if (errorText) throw new Error(`opening ${url}: ${errorText}`);
    await loaded;
    // Fonts and deferred scripts.
    await sleep(500);
    // Only a page the demo served whole is worth a shot: not an error page
    // for a path the seed no longer has.
    const { result, exceptionDetails } = await page.send("Runtime.evaluate", {
      returnByValue: true,
      expression: `(() => {
        const status = performance.getEntriesByType("navigation")[0].responseStatus;
        if (status !== 200) return { status };
        ${prepare ?? ""};
        const stop = ${until ?? "null"};
        const height = stop ? stop.getBoundingClientRect().top + window.scrollY : document.documentElement.scrollHeight;
        return { status, height: Math.ceil(height) };
      })()`,
    });
    if (exceptionDetails) {
      throw new Error(`preparing ${url}: ${exceptionDetails.exception?.description ?? exceptionDetails.text}`);
    }
    const { status, height } = result.value;
    if (status !== 200) throw new Error(`${url} answered ${status}`);
    const { data } = await page.send("Page.captureScreenshot", {
      format: "png",
      captureBeyondViewport: true,
      clip: { x: 0, y: 0, width, height, scale: 1 },
    });
    return Buffer.from(data, "base64");
  } finally {
    page.close();
    // Best effort: if Chrome is gone, so is the tab.
    await fetch(`http://127.0.0.1:${port}/json/close/${target.id}`).catch(() => {});
  }
}

// A DevTools session over `ws`: `send` a method, or wait `once` for an event.
async function connect(ws) {
  const socket = new WebSocket(ws);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = reject;
  });
  let next = 0;
  const pending = new Map();
  const waiting = new Map();
  socket.onmessage = ({ data }) => {
    const msg = JSON.parse(data);
    if (msg.id !== undefined) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
    } else if (waiting.has(msg.method)) {
      waiting.get(msg.method).resolve(msg.params);
      waiting.delete(msg.method);
    }
  };
  // Chrome gone: fail whatever still waits on it, rather than hang.
  const closed = new Error("the DevTools connection closed");
  socket.onclose = () => {
    for (const { reject } of [...pending.values(), ...waiting.values()]) reject(closed);
    pending.clear();
    waiting.clear();
  };
  return {
    send: (method, params = {}) =>
      new Promise((resolve, reject) => {
        if (socket.readyState !== WebSocket.OPEN) return reject(closed);
        const id = next++;
        pending.set(id, { resolve, reject });
        socket.send(JSON.stringify({ id, method, params }));
      }),
    once: (method) => {
      const event = new Promise((resolve, reject) => waiting.set(method, { resolve, reject }));
      // Closing rejects it even when nothing awaits it any more.
      event.catch(() => {});
      return event;
    },
    close: () => socket.close(),
  };
}

// Runs `cmd` to completion, failing if it does; returns its stdout.
function run(cmd, args) {
  const result = spawnSync(cmd, args, { encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], maxBuffer: 1 << 26 });
  if (result.status !== 0) throw new Error(`${cmd} failed: ${result.error ?? `exit ${result.status}`}`);
  return result.stdout;
}

// Asks `child` to stop, and waits for it; kills it if it takes too long.
async function stop(child) {
  if (child.pid === undefined || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolve) => child.once("exit", resolve));
  child.kill("SIGTERM");
  // Unref'd, so it doesn't hold the script open once the child is gone.
  sleep(5000, undefined, { ref: false }).then(() => child.kill("SIGKILL"));
  await exited;
}

try {
  await main();
} catch (err) {
  console.error(err);
  process.exitCode = 1;
} finally {
  for (const cleanup of cleanups.reverse()) {
    try {
      await cleanup();
    } catch (err) {
      console.error(err);
      process.exitCode = 1;
    }
  }
}
