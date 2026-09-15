// The landing page's one clock. A 900ms tick advances the hero graph through
// its eight steps, scrolls the run log, rotates the runs board, and fills the
// chunk field. Under prefers-reduced-motion everything is drawn once, mid-run,
// and never moves. The marquee, the dashed edges, and the blinks are CSS.
import { frame, glyph, logs, RUN_START } from "../landing/demo";
import { fitAll } from "./fit";

const $ = <T extends Element>(sel: string) => document.querySelector<T>(sel);
const $$ = <T extends Element>(sel: string) => Array.from(document.querySelectorAll<T>(sel));

const nodes = $$<HTMLElement>("[data-node]");
const edges = $$<SVGPathElement>("[data-edge]");
const runNo = $("[data-run-no]");
const clocks = $$("[data-clock]");
const log = $("[data-log]");
const boardRows = $("[data-board-rows]");
const boardCount = $("[data-board-count]");
const cells = $$<HTMLElement>("[data-chunk]");
const sessions = $("[data-sessions]");
const stored = $("[data-stored]");

const pad = (n: number) => String(n).padStart(2, "0");
const utc = () => {
  const d = new Date();
  return `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}`;
};

function render(tick: number, step: number, run: number) {
  const f = frame(tick, step);
  nodes.forEach((n, i) => {
    n.classList.toggle("done", i < step);
    n.classList.toggle("active", i === step);
  });
  edges.forEach((e, i) => {
    const loop = i === 6;
    e.classList.toggle("on", loop ? step >= 7 : i < step);
  });
  if (runNo) runNo.textContent = String(run);
  const now = utc();
  clocks.forEach((c) => (c.textContent = now));
  if (log) {
    const lines = f.visibleLogs.map((l, i) => {
      const div = document.createElement("div");
      div.textContent = l;
      if (i < f.visibleLogs.length - 1) div.className = "dim";
      return div;
    });
    log.replaceChildren(...lines);
    if (f.complete) {
      const done = document.createElement("div");
      done.className = "lime";
      done.textContent = "RUN COMPLETE ✓";
      log.append(done);
    } else {
      const cursor = document.createElement("span");
      cursor.className = "cursor";
      log.append(cursor);
    }
  }
  if (boardRows) {
    boardRows.replaceChildren(
      ...f.board.map((r) => {
        const row = document.createElement("div");
        row.className = "row";
        const cell = (cls: string, text: string) => {
          const s = document.createElement("span");
          s.className = cls;
          s.textContent = text;
          return s;
        };
        row.append(
          cell("time", r.time),
          cell("name", r.name),
          cell("trigger", r.trigger),
          cell("dur", r.dur),
          cell(`status ${r.status}`, `${glyph[r.status]} ${r.status}`),
        );
        return row;
      }),
    );
  }
  if (boardCount) boardCount.textContent = "Example runs";
  cells.forEach((c, i) => (c.className = f.chunks[i] ?? ""));
  if (sessions) sessions.textContent = String(f.sessions);
  if (stored) stored.textContent = f.stored;
}

const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
if (reduced) {
  render(5, 6, RUN_START);
} else {
  let tick = 0;
  let step = 0;
  let run = RUN_START;
  render(tick, step, run);
  setInterval(() => {
    tick += 1;
    if (step + 1 > 8) {
      step = 0;
      run += 1;
    } else {
      step += 1;
    }
    render(tick, step, run);
  }, 900);
}
void logs;
fitAll();
