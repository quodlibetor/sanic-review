// Screenshots of the demo dashboard for the README: builds and starts the
// `demo` example of sanic-web, drives headless Chrome over the DevTools
// protocol, and writes docs/images/*.png. `mise run screenshots` runs it.
//
// Chrome is `$CHROME`, else `google-chrome` on PATH. PNGs go through
// oxipng when it's installed.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as sleep } from "node:timers/promises";

const OUT = "docs/images";
const SHOWCASE = "/pr/quodlibetor/frobnicator/42";

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
  const demo = await startDemo();
  const port = await startChrome();
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

// Starts headless Chrome with a fresh profile; resolves with its DevTools
// port.
async function startChrome() {
  const tmp = mkdtempSync(join(tmpdir(), "sanic-screenshots-"));
  // Added first, so it runs once Chrome has stopped writing to it.
  cleanups.push(() => rmSync(tmp, { recursive: true, force: true }));
  const profile = join(tmp, "profile");
  const child = spawn(
    process.env.CHROME || "google-chrome",
    [
      "--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`,
      "--no-first-run", "--no-default-browser-check", "--hide-scrollbars", "--disable-gpu",
      "about:blank",
    ],
    { stdio: "ignore" },
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
