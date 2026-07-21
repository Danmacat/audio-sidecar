#!/usr/bin/env node
// Zero-dependency driver for audio-sidecar. Spawns the sidecar, speaks the
// NDJSON protocol, and offers small verification subcommands:
//
//   node tools/dev-client.mjs hello
//   node tools/dev-client.mjs devices
//   node tools/dev-client.mjs apps
//   node tools/dev-client.mjs watch
//   node tools/dev-client.mjs meter [--default|--input|--device <id>|--pid <n> [--exclude]]
//                                   [--bands <n>] [--fps <n>] [--stall <ms>]
//   node tools/dev-client.mjs media [--watch] [--artwork <file>]
//   node tools/dev-client.mjs pcm   [--default|...] [--seconds <n>] [--out <file>] [--f32]
//   node tools/dev-client.mjs raw '<json>'
//
// Env: SIDECAR_BIN (path to exe), SIDECAR_LOG (log level, default warn).

import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const BIN =
  process.env.SIDECAR_BIN ??
  path.join(here, "..", "target", "debug", process.platform === "win32" ? "audio-sidecar.exe" : "audio-sidecar");

const argv = process.argv.slice(2);
const cmd = argv[0];
const flags = {};
for (let i = 1; i < argv.length; i++) {
  const a = argv[i];
  if (!a.startsWith("--")) {
    (flags._ ??= []).push(a);
    continue;
  }
  const key = a.slice(2);
  const next = argv[i + 1];
  if (next !== undefined && !next.startsWith("--")) {
    flags[key] = next;
    i++;
  } else {
    flags[key] = true;
  }
}

class Client {
  constructor() {
    if (!fs.existsSync(BIN)) {
      console.error(`sidecar binary not found at ${BIN} — run "cargo build" first (or set SIDECAR_BIN)`);
      process.exit(1);
    }
    this.child = spawn(BIN, ["--log-level", process.env.SIDECAR_LOG ?? "warn"], {
      stdio: ["pipe", "pipe", "inherit"],
    });
    this.child.on("exit", (code) => {
      if (!this.closing) {
        console.error(`sidecar exited unexpectedly (code ${code})`);
        process.exit(1);
      }
    });
    this.pending = new Map();
    this.nextId = 1;
    this.handlers = [];
    this.closing = false;
    this.rl = createInterface({ input: this.child.stdout });
    this.rl.on("line", (line) => this.onLine(line));
  }

  onLine(line) {
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      console.error("unparseable line from sidecar:", line);
      return;
    }
    if (msg.event !== undefined) {
      for (const h of this.handlers) h(msg.event, msg.data);
      return;
    }
    const p = this.pending.get(msg.id);
    if (!p) return;
    this.pending.delete(msg.id);
    if (msg.error) {
      const err = new Error(`${msg.error.code}: ${msg.error.message}`);
      err.rpc = msg.error;
      p.reject(err);
    } else {
      p.resolve(msg.result);
    }
  }

  call(method, params = {}) {
    const id = this.nextId++;
    this.child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }

  on(handler) {
    this.handlers.push(handler);
  }

  async close() {
    this.closing = true;
    try {
      await Promise.race([this.call("shutdown"), sleep(1000)]);
    } catch {}
    this.child.stdin.end();
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function sourceFromFlags() {
  if (flags.device) return { type: "device", deviceId: flags.device };
  if (flags.pid && flags.exclude) return { type: "systemExcludingProcess", pid: Number(flags.pid) };
  if (flags.pid) return { type: "process", pid: Number(flags.pid) };
  if (flags.input) return { type: "defaultInput" };
  return { type: "defaultOutput" };
}

function onExitSignals(fn) {
  process.on("SIGINT", fn);
  process.on("SIGTERM", fn);
}

const BLOCKS = " ▁▂▃▄▅▆▇█";
function bar(values) {
  return values.map((v) => BLOCKS[Math.min(8, Math.max(0, Math.floor(v * 8.999)))]).join("");
}

async function main() {
  switch (cmd) {
    case "hello": {
      const c = new Client();
      console.log(JSON.stringify(await c.call("hello", { client: { name: "dev-client", version: "0.1" } }), null, 2));
      await c.close();
      break;
    }

    case "devices": {
      const c = new Client();
      const { devices } = await c.call("devices.list");
      for (const d of devices) {
        const tags = [
          d.kind,
          d.state,
          d.isDefault ? "DEFAULT" : null,
          d.isDefaultCommunications ? "default-comm" : null,
          d.format ? `${d.format.sampleRate}Hz/${d.format.channels}ch` : null,
        ].filter(Boolean);
        console.log(`${d.name}\n  [${tags.join(", ")}]\n  id: ${d.id}`);
      }
      await c.close();
      break;
    }

    case "apps": {
      const c = new Client();
      const { processes } = await c.call("processes.listAudio");
      if (!processes.length) console.log("(no processes currently have audio sessions)");
      for (const p of processes) {
        console.log(`${String(p.pid).padStart(6)}  ${p.state.padEnd(8)}  ${p.name}${p.executable ? `  (${p.executable})` : ""}`);
      }
      await c.close();
      break;
    }

    case "watch": {
      const c = new Client();
      await c.call("hello");
      console.error("watching events; Ctrl+C to quit");
      c.on((event, data) => console.log(JSON.stringify({ event, data })));
      onExitSignals(async () => {
        await c.close();
        process.exit(0);
      });
      break;
    }

    case "meter": {
      const c = new Client();
      const source = sourceFromFlags();
      const spectrum = {
        bands: Number(flags.bands ?? 64),
        fps: Number(flags.fps ?? 30),
      };
      c.on((event, data) => {
        if (event === "capture.state") console.error(`[state] ${JSON.stringify(data)}`);
      });
      const started = await c.call("capture.start", { source, spectrum });
      console.error(
        `capturing ${JSON.stringify(source)} -> ${started.format.sampleRate}Hz/${started.format.channels}ch, ` +
          `${started.spectrum.bands} bands @ ${started.spectrum.fps}fps (captureId ${started.captureId})`
      );
      let lastSeq = -1;
      let gaps = 0;
      let painted = false;
      c.on((event, data) => {
        if (event !== "capture.spectrum") return;
        if (lastSeq >= 0 && data.seq !== lastSeq + 1) gaps += data.seq - lastSeq - 1;
        lastSeq = data.seq;
        const lines = data.bands.map(
          (ch, i) => `${i === 0 ? "L" : "R"} |${bar(ch)}| rms=${(data.rms[i] ?? 0).toFixed(3)}`
        );
        lines.push(`seq=${data.seq} droppedGaps=${gaps}   Ctrl+C to quit`);
        const text = lines.join("\n");
        process.stdout.write((painted ? `\x1b[${lines.length}F` : "") + text.replace(/$/gm, "\x1b[K") + "\n");
        painted = true;
      });
      if (flags.stall) {
        const ms = Number(flags.stall);
        setTimeout(() => {
          console.error(`\n[stall] pausing reads for ${ms} ms to test backpressure`);
          c.child.stdout.pause();
          setTimeout(() => {
            c.child.stdout.resume();
            console.error("[stall] resumed");
          }, ms);
        }, 3000);
      }
      onExitSignals(async () => {
        process.stdout.write("\n");
        await c.close();
        process.exit(0);
      });
      break;
    }

    case "media": {
      const c = new Client();
      const snap = await c.call("media.getSessions");
      console.log(JSON.stringify(snap, null, 2));
      if (flags.artwork) {
        const target = snap.sessions.find((s) => s.isCurrent && s.artworkAvailable) ?? snap.sessions.find((s) => s.artworkAvailable);
        if (!target) {
          console.error("no session with artwork available");
        } else {
          const art = await c.call("media.getArtwork", { sessionId: target.sessionId });
          fs.writeFileSync(flags.artwork, Buffer.from(art.dataBase64, "base64"));
          console.error(`wrote ${art.byteLength} bytes (${art.contentType}) for "${target.title}" -> ${flags.artwork}`);
        }
      }
      if (flags.watch) {
        console.error("watching media events; Ctrl+C to quit");
        c.on((event, data) => {
          if (event.startsWith("media.")) console.log(JSON.stringify({ event, data }));
        });
        onExitSignals(async () => {
          await c.close();
          process.exit(0);
        });
      } else {
        await c.close();
      }
      break;
    }

    case "pcm": {
      const c = new Client();
      const source = sourceFromFlags();
      const seconds = Number(flags.seconds ?? 5);
      const format = flags.f32 ? "f32le" : "s16le";
      const out = flags.out ?? "capture.raw";
      const chunks = [];
      let info = null;
      c.on((event, data) => {
        if (event === "capture.pcm") chunks.push(Buffer.from(data.dataBase64, "base64"));
      });
      info = await c.call("capture.start", {
        source,
        spectrum: { enabled: false },
        pcm: { enabled: true, format },
      });
      console.error(`recording ${seconds}s of ${info.format.sampleRate}Hz/${info.format.channels}ch ${format}...`);
      await sleep(seconds * 1000);
      await c.call("capture.stop", { captureId: info.captureId });
      fs.writeFileSync(out, Buffer.concat(chunks));
      console.error(
        `wrote ${out} — import in Audacity as raw: ${format === "s16le" ? "Signed 16-bit PCM" : "32-bit float"}, ` +
          `little-endian, ${info.format.channels} channels, ${info.format.sampleRate} Hz`
      );
      await c.close();
      break;
    }

    case "raw": {
      const c = new Client();
      const req = JSON.parse(flags._?.[0] ?? argv[1]);
      c.on((event, data) => console.error(`[event] ${JSON.stringify({ event, data })}`));
      try {
        console.log(JSON.stringify(await c.call(req.method, req.params ?? {}), null, 2));
      } catch (e) {
        console.error(String(e.rpc ? JSON.stringify(e.rpc) : e));
      }
      await c.close();
      break;
    }

    default:
      console.error("usage: node tools/dev-client.mjs <hello|devices|apps|watch|meter|media|pcm|raw> [flags]");
      process.exit(2);
  }
}

main().catch((e) => {
  console.error(e.rpc ? `RPC error: ${JSON.stringify(e.rpc)}` : e);
  process.exit(1);
});
