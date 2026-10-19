// 개발자 도구 창. 가상 드라이브에서 열린 파일을 최근 것부터 보여준다.
// 경로·프로세스 이름은 textContent로만 넣는다.
"use strict";

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

const POLL_MS = 500;
const MAX_ROWS = 1000;

/** 마지막으로 받은 번호 */
let lastSeq = 0;
/** 받은 기록. 최근 것이 뒤 */
let entries = [];

function bytes(n) {
  const units = ["B", "KB", "MB", "GB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${units[i]}`;
}

function timeText(ms) {
  const d = new Date(ms);
  const p = (n, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

function processName(path) {
  if (!path) return "?";
  return path.split(/[\\/]/).pop();
}

function matches(e, q) {
  if (!q) return true;
  return e.path.toLowerCase().includes(q) || (e.process ?? "").toLowerCase().includes(q);
}

function row(e) {
  const tr = document.createElement("tr");
  const cells = [
    timeText(e.at_ms),
    `${processName(e.process)} (${e.pid})`,
    e.path,
    bytes(e.size),
  ];
  for (const text of cells) {
    const td = document.createElement("td");
    td.textContent = text;
    tr.append(td);
  }
  tr.children[1].title = e.process ?? "";
  tr.children[2].title = e.path;
  return tr;
}

function render() {
  const q = $("filter").value.trim().toLowerCase();
  const shown = entries.filter((e) => matches(e, q));
  const body = $("rows");
  body.replaceChildren(...shown.slice().reverse().map(row));
  $("count").textContent = q ? `${shown.length} / ${entries.length}` : `${entries.length}`;
}

async function poll() {
  if ($("pause").checked) return;
  const fresh = await invoke("devtools_opens", { after: lastSeq });
  if (fresh.length === 0) return;
  lastSeq = fresh[fresh.length - 1].seq;
  entries = entries.concat(fresh).slice(-MAX_ROWS);
  render();
}

$("filter").addEventListener("input", render);
$("clear").addEventListener("click", async () => {
  await invoke("devtools_clear");
  entries = [];
  render();
});

render();
setInterval(() => poll().catch(() => {}), POLL_MS);
