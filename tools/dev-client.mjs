#!/usr/bin/env node
// Zero-dependency driver for audio-sidecar. Spawns the sidecar, speaks the
// NDJSON protocol, and offers small verification subcommands:
//
//   node tools/dev-client.mjs hello
//   node tools/dev-client.mjs devices
//   node tools/dev-client.mjs apps
//   node tools/dev-client.mjs watch
//   node tools/dev-client.mjs meter [sources...] [--bands <n>] [--fps <n>] [--stall <ms>]
//   node tools/dev-client.mjs media [--watch] [--artwork <file>] [--artwork-to <dir>] [--artwork-dir <dir>]
//   node tools/dev-client.mjs pcm   [source] [--seconds <n>] [--out <file>] [--f32]
//   node tools/dev-client.mjs raw '<json>'
//
// Source flags (repeatable — `meter` runs ALL of them concurrently in ONE
// sidecar process, demonstrating multi-capture):
//   --default             follow default output      --input   default input
//   --device <id>         a specific endpoint
//   --pid <n>             one process (+children)    --exclude-pid <n>  everything but it
//   (legacy: --pid <n> --exclude == --exclude-pid <n>)
//
// Example: node tools/dev-client.mjs meter --pid 1234 --pid 5678 --default
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
/** flags[key] is an array of values ("true" markers for bare flags). */
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
    (flags[key] ??= []).push(next);
    i++;
  } else {
    (flags[key] ??= []).push(true);
  }
}
const flag1 = (k) => flags[k]?.[0];
const has = (k) => flags[k] !== undefined;

class Client {
  constructor(extraArgs = []) {
    if (!fs.existsSync(BIN)) {
      console.error(`sidecar binary not found at ${BIN} — run "cargo build" first (or set SIDECAR_BIN)`);
      process.exit(1);
    }
    this.child = spawn(BIN, ["--log-level", process.env.SIDECAR_LOG ?? "warn", ...extraArgs], {
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

function sourcesFromFlags() {
  const out = [];
  for (const id of flags.device ?? []) out.push({ type: "device", deviceId: id });
  const excludeAll = has("exclude"); // legacy: --pid N --exclude
  for (const p of flags.pid ?? [])
    out.push(
      excludeAll
        ? { type: "systemExcludingProcess", pid: Number(p) }
        : { type: "process", pid: Number(p) }
    );
  for (const p of flags["exclude-pid"] ?? []) out.push({ type: "systemExcludingProcess", pid: Number(p) });
  if (has("default")) out.push({ type: "defaultOutput" });
  if (has("input")) out.push({ type: "defaultInput" });
  if (!out.length) out.push({ type: "defaultOutput" });
  return out;
}

function sourceLabel(src) {
  switch (src.type) {
    case "device":
      return `device:${src.deviceId.slice(-9, -1)}`;
    case "process":
      return `pid:${src.pid}`;
    case "systemExcludingProcess":
      return `!pid:${src.pid}`;
    case "defaultInput":
      return "input";
    default:
      return "default";
  }
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
      const sources = sourcesFromFlags();
      const spectrum = {
        bands: Number(flag1("bands") ?? 64),
        fps: Number(flag1("fps") ?? 30),
      };
      c.on((event, data) => {
        if (event === "capture.state") console.error(`[state] ${JSON.stringify(data)}`);
      });

      // All sources run concurrently in this ONE sidecar process; frames are
      // demultiplexed by captureId.
      const caps = new Map(); // captureId -> render state
      for (const source of sources) {
        const started = await c.call("capture.start", { source, spectrum });
        caps.set(started.captureId, {
          label: sourceLabel(source),
          bands: null,
          rms: [],
          seq: -1,
          gaps: 0,
        });
        console.error(
          `${started.captureId} <- ${sourceLabel(source)} @ ${started.format.sampleRate}Hz/${started.format.channels}ch`
        );
      }

      const single = caps.size === 1;
      let painted = 0;
      const repaint = () => {
        const lines = [];
        for (const [id, s] of caps) {
          if (!s.bands) continue;
          if (single) {
            s.bands.forEach((ch, i) =>
              lines.push(`${i === 0 ? "L" : "R"} |${bar(ch)}| rms=${(s.rms[i] ?? 0).toFixed(3)}`)
            );
          } else {
            // One mixed row per capture: element-wise max of both channels.
            const mixed = s.bands[0].map((v, i) => Math.max(v, s.bands[1]?.[i] ?? 0));
            lines.push(`${id} ${s.label.padEnd(12)} |${bar(mixed)}| rms=${(s.rms[0] ?? 0).toFixed(3)}`);
          }
        }
        const gaps = [...caps.values()].reduce((n, s) => n + s.gaps, 0);
        lines.push(`captures=${caps.size} droppedGaps=${gaps}   Ctrl+C to quit`);
        const text = lines.map((l) => l + "\x1b[K").join("\n");
        process.stdout.write((painted ? `\x1b[${painted}F` : "") + text + "\n");
        painted = lines.length;
      };
      c.on((event, data) => {
        if (event !== "capture.spectrum") return;
        const s = caps.get(data.captureId);
        if (!s) return;
        if (s.seq >= 0 && data.seq !== s.seq + 1) s.gaps += data.seq - s.seq - 1;
        s.seq = data.seq;
        s.bands = data.bands;
        s.rms = data.rms;
        repaint();
      });

      if (has("stall")) {
        const ms = Number(flag1("stall"));
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
      const extra = has("artwork-dir") ? ["--artwork-dir", flag1("artwork-dir")] : [];
      const c = new Client(extra);
      const snap = await c.call("media.getSessions");
      console.log(JSON.stringify(snap, null, 2));
      if (has("artwork") || has("artwork-to")) {
        const target =
          snap.sessions.find((s) => s.isCurrent && s.artworkAvailable) ??
          snap.sessions.find((s) => s.artworkAvailable);
        if (!target) {
          console.error("no session with artwork available");
        } else if (has("artwork-to")) {
          // Pull-mode file cache: sidecar writes the file, we get the path.
          const art = await c.call("media.getArtwork", {
            sessionId: target.sessionId,
            writeTo: flag1("artwork-to"),
          });
          console.error(
            `sidecar cached ${art.byteLength} bytes (${art.contentType}) for "${target.title}" -> ${art.file} (hash ${art.hash})`
          );
        } else {
          const art = await c.call("media.getArtwork", { sessionId: target.sessionId });
          fs.writeFileSync(flag1("artwork"), Buffer.from(art.dataBase64, "base64"));
          console.error(`wrote ${art.byteLength} bytes (${art.contentType}) for "${target.title}" -> ${flag1("artwork")}`);
        }
      }
      if (has("watch")) {
        console.error("watching media events; Ctrl+C to quit");
        c.on((event, data) => {
          if (event.startsWith("media.")) console.log(JSON.stringify({ event, data }));
        });
        onExitSignals(async () => {
          await c.close();
          process.exit(0);
        });
      } else if (has("artwork-dir")) {
        // Give background artwork caching a moment, then show the results.
        await sleep(2000);
        const after = await c.call("media.getSessions");
        for (const s of after.sessions) {
          if (s.artworkFile) console.error(`artwork cached: ${s.sessionId} -> ${s.artworkFile}`);
        }
        await c.close();
      } else {
        await c.close();
      }
      break;
    }

    case "pcm": {
      const c = new Client();
      const source = sourcesFromFlags()[0];
      const seconds = Number(flag1("seconds") ?? 5);
      const format = has("f32") ? "f32le" : "s16le";
      const out = flag1("out") ?? "capture.raw";
      const chunks = [];
      c.on((event, data) => {
        if (event === "capture.pcm") chunks.push(Buffer.from(data.dataBase64, "base64"));
      });
      const info = await c.call("capture.start", {
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
