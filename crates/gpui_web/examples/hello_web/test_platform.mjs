// Serve the platform_test Wasm fixture with shared-memory isolation headers.
// GPUI_WEB_TEST_OUTPUT=/path/to/artifacts node test_platform.mjs http://127.0.0.1:8080
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";

const origin = new URL(process.argv[2] ?? "http://127.0.0.1:8080").origin;
const chrome = process.env.CHROME ??
    (process.platform === "darwin"
        ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        : "chromium");
const output = process.env.GPUI_WEB_TEST_OUTPUT;
assert.ok(output, "GPUI_WEB_TEST_OUTPUT must name a private artifact directory");
fs.mkdirSync(output, { recursive: true });
const log = fs.openSync(path.join(output, "chrome.log"), "w");
const profile = fs.mkdtempSync(path.join(output, "profile-"));
const browser = spawn(chrome, [
    "--headless=new", "--no-first-run", "--no-default-browser-check",
    "--disable-background-networking", "--disable-sync", "--disable-extensions",
    "--password-store=basic",
    "--remote-debugging-port=0", `--user-data-dir=${profile}`,
    "about:blank",
], { stdio: ["ignore", log, log] });
const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
let launchError;
browser.on("error", error => { launchError = error; });
let socket;
try {
    let port;
    for (let attempt = 0; attempt < 100; attempt++) {
        if (launchError) throw launchError;
        if (browser.exitCode !== null || browser.signalCode !== null) throw new Error(`Chrome exited; see ${output}/chrome.log`);
        const portFile = path.join(profile, "DevToolsActivePort");
        if (fs.existsSync(portFile)) {
            port = fs.readFileSync(portFile, "utf8").split("\n")[0];
            break;
        }
        await delay(100);
    }
    assert.ok(port, "Chrome did not start");
    const pages = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
    socket = new WebSocket(pages.find(page => page.type === "page").webSocketDebuggerUrl);
    await new Promise((resolve, reject) => {
        socket.addEventListener("open", resolve, { once: true });
        socket.addEventListener("error", reject, { once: true });
    });
    let nextId = 0;
    const pending = new Map();
    const errors = [];
    socket.addEventListener("message", ({ data }) => {
        const message = JSON.parse(data);
        if (message.id) {
            const request = pending.get(message.id);
            if (!request) return;
            pending.delete(message.id);
            clearTimeout(request.timer);
            if (message.error) request.reject(new Error(JSON.stringify(message.error)));
            else request.resolve(message.result);
        } else if (message.method === "Runtime.exceptionThrown") {
            errors.push(message.params);
        }
    });
    const call = (method, params = {}) => new Promise((resolve, reject) => {
        const id = ++nextId;
        // This deadline runs outside the page, so it also catches a main-thread hang.
        const timer = setTimeout(() => reject(new Error(`Timed out: ${method}`)), 120_000);
        timer.unref();
        pending.set(id, { resolve, reject, timer });
        socket.send(JSON.stringify({ id, method, params }));
    });
    await call("Runtime.enable");
    await call("Page.enable");
    const loaded = new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("Page did not load")), 10_000);
        socket.addEventListener("message", function onLoad({ data }) {
            if (JSON.parse(data).method === "Page.loadEventFired") {
                clearTimeout(timer);
                socket.removeEventListener("message", onLoad);
                resolve();
            }
        });
    });
    await call("Page.navigate", { url: origin });
    await loaded;
    const result = await call("Runtime.evaluate", {
        awaitPromise: true, returnByValue: true,
        expression: `(async () => {
            if (!crossOriginIsolated) throw new Error("Shared-memory isolation headers are required");
            const fixture = await import("/platform_test.js");
            await fixture.default();
            fixture.verify_window_background_capabilities();
            return true;
        })()`,
    });
    assert.equal(result.exceptionDetails, undefined, JSON.stringify(result.exceptionDetails));
    assert.deepEqual(errors, [], JSON.stringify(errors));
    assert.equal(result.result.value, true);
    console.log("PASS: browser window background capabilities");
} finally {
    socket?.close();
    browser.kill();
    fs.closeSync(log);
}
