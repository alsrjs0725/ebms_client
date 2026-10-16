// 설정 창. 서버 목록·로그인·계정 정보·캐시·가상 드라이브 위치·구동기.
// 서버가 보낸 문자열은 모두 textContent로만 넣는다.
"use strict";

const invoke = window.__TAURI__.core.invoke;
const $ = (id) => document.getElementById(id);

const OAUTH_NAMES = { google: "Google", discord: "Discord" };
const LOGIN_LABELS = {
  logged_in: "로그인됨",
  needs_login: "다시 로그인 필요",
  logged_out: "로그아웃 상태",
};
const REFRESH_MS = 30_000;
const INSECURE_TEXT = "암호화되지 않은 http 주소입니다. 세션키가 평문으로 전송됩니다.";

/** 진행 중인 로그인 (서버 id) */
const loggingIn = new Set();
/** 사전 다운로드 범위 창에서 고르는 중인 서버 */
let preServer = null;
/** 개인 채보 창을 연 서버 */
let personalServer = null;
const SEARCH_DELAY_MS = 200;
/** 사전 다운로드 범위 창에서 쓰는 받을 용량(`pending_download`). 계산 중이면 "loading", 모르면 null */
let prePending = null;
/** 서버 카드에 보여 줄 받을 용량. 서버 id → { key, at, value } */
const pendingCache = new Map();
const PENDING_TTL_MS = 5 * 60_000;

function showMessage(text, kind = "error") {
  const el = $("message");
  el.textContent = text;
  el.className = `message ${kind === "info" ? "info" : ""}`;
  el.hidden = !text;
}

function bytes(n) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${units[i]}`;
}

/** PC 시간대 기준 "2026-10-15 13:34". 웹뷰 로캘 설정에 따라 UTC로 보이지 않게 직접 만든다. */
function timeText(unix) {
  const d = new Date(unix * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function addRow(dl, label, value) {
  const dt = document.createElement("dt");
  dt.textContent = label;
  const dd = document.createElement("dd");
  dd.textContent = value;
  dl.append(dt, dd);
}

/** 한 번 더 눌러야 실행한다 (웹뷰마다 confirm 지원이 달라 직접 만든다). */
function confirmClick(button, label, action) {
  const original = button.textContent;
  let armed = false;
  let timer;
  button.addEventListener("click", () => {
    if (!armed) {
      armed = true;
      button.textContent = label;
      timer = setTimeout(() => {
        armed = false;
        button.textContent = original;
      }, 4000);
      return;
    }
    clearTimeout(timer);
    armed = false;
    button.textContent = original;
    action();
  });
}

/** 버튼을 누르는 동안 막고, 오류는 메시지로 보여준다. */
async function run(button, action) {
  if (button) button.disabled = true;
  try {
    await action();
  } catch (e) {
    showMessage(String(e));
  } finally {
    if (button) button.disabled = false;
  }
}

function renderServer(s) {
  const li = $("server-template").content.firstElementChild.cloneNode(true);
  li.querySelector(".name").textContent = s.name;
  li.querySelector(".url").textContent = s.url;
  li.querySelector(".insecure").hidden = !s.insecure;

  const badge = li.querySelector(".badge");
  badge.textContent = loggingIn.has(s.id) ? "브라우저에서 로그인 중…" : LOGIN_LABELS[s.login];
  badge.classList.add(s.login);

  const dl = li.querySelector(".account");
  if (s.me) {
    const me = s.me;
    addRow(dl, "계정", me.display_name);
    const oauths = me.oauths.map((o) => OAUTH_NAMES[o.oauth] || o.oauth);
    addRow(dl, "로그인 수단", oauths.length ? `${oauths.join(", ")}로 로그인 가능` : "-");
    const t = me.tickets;
    let tickets = `${t.available} / ${t.max}`;
    if (t.next_refill_at) {
      const secs = Math.max(0, t.next_refill_at - Math.floor(Date.now() / 1000));
      tickets += ` (다음 티켓 ${secs}초 후)`;
    }
    addRow(dl, "플레이 티켓", tickets);
    const p = me.pre;
    let pre = `${bytes(p.used_bytes)} / ${bytes(p.limit_bytes)}`;
    if (p.throttled) pre += ` (한도 초과, ${p.throttled_kbps} Kbps로 감속 중)`;
    addRow(dl, `사전 다운로드 (${p.month})`, pre);
  } else if (s.error) {
    addRow(dl, "상태", `서버에 연결하지 못했습니다: ${s.error}`);
  }
  dl.hidden = !dl.children.length;
  li.querySelector(".session-file").hidden = !s.session_in_file;

  const sync = li.querySelector(".sync");
  const syncState = li.querySelector(".sync-state");
  const last = s.sync.last_ok_at ? `마지막 동기화 ${timeText(s.sync.last_ok_at)}` : "";
  if (s.sync.running) {
    syncState.textContent = "동기화 중…";
    syncState.className = "sync-state running";
    sync.textContent = last;
  } else if (s.sync.last_error) {
    syncState.textContent = "동기화 실패";
    syncState.className = "sync-state failed";
    sync.textContent = last ? `${s.sync.last_error} · ${last}` : s.sync.last_error;
  } else if (s.sync.last_ok_at) {
    syncState.textContent = "동기화 완료";
    syncState.className = "sync-state done";
    sync.textContent = last;
  }
  syncState.hidden = !syncState.textContent;
  sync.hidden = !sync.textContent;

  const scope = li.querySelector(".pre-scope");
  scope.textContent = s.pre_tables
    ? `사전 다운로드: 난이도표 ${s.pre_tables.length}개의 곡`
    : "사전 다운로드: 전곡";
  scope.hidden = s.login !== "logged_in";
  if (!scope.hidden) {
    const base = scope.textContent;
    cardPending(s)
      .then((p) => {
        if (p) scope.textContent = `${base} · 다음 동기화에 받을 용량 ${pendingText(p, s.pre_tables)}`;
      })
      .catch(() => {});
  }

  const usage = li.querySelector(".usage");
  const u = s.usage;
  if (u) {
    usage.textContent = `캐시 ${bytes(u.cache_bytes)} / ${bytes(u.cache_limit)} · 로컬 데이터 전체 ${bytes(u.total_bytes)}`;
  }
  usage.hidden = !u;

  const loginBtn = li.querySelector(".login");
  const logoutBtn = li.querySelector(".logout");
  const linkBtn = li.querySelector(".account-link");
  const syncBtn = li.querySelector(".sync-now");
  loginBtn.hidden = s.login === "logged_in";
  loginBtn.disabled = loggingIn.has(s.id);
  logoutBtn.hidden = s.login === "logged_out";
  linkBtn.hidden = s.login !== "logged_in";
  syncBtn.hidden = s.login !== "logged_in";
  const preBtn = li.querySelector(".pre-tables");
  preBtn.hidden = s.login !== "logged_in";
  preBtn.addEventListener("click", () => run(preBtn, () => openPreDialog(s)));
  const personalBtn = li.querySelector(".personal");
  personalBtn.addEventListener("click", () => run(personalBtn, () => openPersonalDialog(s)));

  loginBtn.addEventListener("click", async () => {
    loggingIn.add(s.id);
    showMessage("브라우저에서 로그인해 주세요. 끝나면 이 창으로 돌아오면 됩니다.", "info");
    await refresh();
    try {
      await invoke("login", { id: s.id });
      showMessage("");
    } catch (e) {
      showMessage(`로그인하지 못했습니다: ${e}`);
    } finally {
      loggingIn.delete(s.id);
      await refresh();
    }
  });
  logoutBtn.addEventListener("click", () =>
    run(logoutBtn, async () => {
      await invoke("logout", { id: s.id });
      await refresh();
    }),
  );
  linkBtn.addEventListener("click", () => run(linkBtn, () => invoke("open_account", { id: s.id })));
  syncBtn.addEventListener("click", () =>
    run(syncBtn, async () => {
      syncState.textContent = "동기화 중…";
      syncState.className = "sync-state running";
      syncState.hidden = false;
      await invoke("sync_now", { id: s.id });
      await refresh();
    }),
  );
  const clearBtn = li.querySelector(".clear-cache");
  clearBtn.disabled = !u || u.cache_bytes === 0;
  confirmClick(clearBtn, "한 번 더 누르면 캐시 비우기", () =>
    run(clearBtn, async () => {
      const freed = await invoke("clear_cache", { id: s.id });
      showMessage(`${s.name}: 캐시 ${bytes(freed)}를 비웠습니다.`, "info");
      await refresh();
    }),
  );
  const removeBtn = li.querySelector(".remove");
  confirmClick(removeBtn, "한 번 더 누르면 삭제 (로컬 데이터는 남김)", () =>
    run(removeBtn, async () => {
      await invoke("remove_server", { id: s.id, purge: false });
      await refresh();
    }),
  );
  const purgeBtn = li.querySelector(".remove-purge");
  if (u) purgeBtn.textContent = `삭제 + 로컬 데이터 지우기 (${bytes(u.total_bytes)})`;
  confirmClick(purgeBtn, "한 번 더 누르면 삭제하고 로컬 데이터도 지움", () =>
    run(purgeBtn, async () => {
      await invoke("remove_server", { id: s.id, purge: true });
      await refresh();
    }),
  );
  return li;
}

/** 고른 난이도표(null이면 전곡)로 동기화하면 받을 { chart, pre, total } 바이트. 서버의 sync_pre와 같은 기준. */
function pendingBytes(p, selected) {
  const pick = selected && p.tables_known ? new Set(selected) : null;
  const pre = p.pre
    .filter((c) => !pick || c.tables.some((t) => pick.has(t)))
    .reduce((sum, c) => sum + c.size, 0);
  return { chart: p.chart_bytes, pre, total: p.chart_bytes + pre };
}

function pendingText(p, selected) {
  const b = pendingBytes(p, selected);
  if (b.total === 0) return "없음 (이미 모두 받음)";
  return `약 ${bytes(b.total)} (사전 파일 ${bytes(b.pre)} · 차트 ${bytes(b.chart)})`;
}

/** 서버 카드의 받을 용량. 동기화가 끝나면(last_ok_at이 바뀌면) 다시 센다. */
async function cardPending(s) {
  const key = `${s.sync.last_ok_at}|${JSON.stringify(s.pre_tables)}`;
  const hit = pendingCache.get(s.id);
  if (hit && hit.key === key && Date.now() - hit.at < PENDING_TTL_MS) return hit.value;
  const value = await invoke("pending_download", { id: s.id });
  pendingCache.set(s.id, { key, at: Date.now(), value });
  return value;
}

/** 사전 다운로드 범위 창. 서버의 난이도표를 받아 고르게 한다. */
async function openPreDialog(s) {
  const pendingReq = invoke("pending_download", { id: s.id }).catch(() => null);
  const view = await invoke("pre_tables", { id: s.id });
  preServer = s.id;
  prePending = "loading";
  $("pre-pending").textContent = "받을 용량을 계산하는 중…";
  $("pre-pending").hidden = false;
  pendingReq.then((p) => {
    if (preServer !== s.id) return;
    prePending = p;
    updatePreForm();
  });
  $("pre-title").textContent = `사전 다운로드 범위 · ${s.name}`;
  const form = $("pre-form");
  const selected = new Set(view.selected || []);
  form.scope.value = view.selected ? "tables" : "all";
  const items = view.tables.map((t) => {
    const li = document.createElement("li");
    const label = document.createElement("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.value = String(t.id);
    box.checked = selected.has(t.id);
    const count = document.createElement("span");
    count.className = "muted";
    count.textContent = ` (${t.songs}곡)`;
    label.append(box, ` ${t.symbol} ${t.name}`, count);
    li.append(label);
    return li;
  });
  $("pre-list").replaceChildren(...items);
  const empty = $("pre-empty");
  empty.textContent = view.available
    ? "서버에 등록된 난이도표가 없습니다."
    : "이 서버는 난이도표를 지원하지 않아 전곡을 받습니다.";
  empty.hidden = view.tables.length > 0;
  form.querySelector('[value="tables"]').disabled = view.tables.length === 0;
  updatePreForm();
  // Esc로 닫으면 returnValue가 바뀌지 않으므로 지난 값을 지운다.
  $("pre-dialog").returnValue = "";
  $("pre-dialog").showModal();
}

function updatePreForm() {
  const form = $("pre-form");
  const byTable = form.scope.value === "tables";
  const boxes = [...$("pre-list").querySelectorAll("input")];
  boxes.forEach((b) => {
    b.disabled = !byTable;
  });
  const checked = boxes.filter((b) => b.checked).map((b) => Number(b.value));
  $("pre-save").disabled = byTable && !checked.length;
  // 계산 중이면 그대로 두고, 이전 서버라 크기를 모르면 숨긴다.
  const pending = $("pre-pending");
  if (prePending === "loading") return;
  pending.hidden = !prePending;
  if (prePending) pending.textContent = `저장하면 받을 용량: ${pendingText(prePending, byTable ? checked : null)}`;
}

$("pre-form").addEventListener("change", updatePreForm);

$("pre-dialog").addEventListener("close", () => {
  const id = preServer;
  preServer = null;
  prePending = null;
  if ($("pre-dialog").returnValue !== "save" || !id) return;
  const form = $("pre-form");
  const selected =
    form.scope.value === "tables"
      ? [...$("pre-list").querySelectorAll("input:checked")].map((b) => Number(b.value))
      : null;
  showMessage("사전 다운로드 범위를 바꿨습니다. 동기화하는 중입니다…", "info");
  pendingCache.delete(id);
  invoke("set_pre_tables", { id, selected })
    .then(() => showMessage("사전 다운로드 범위를 바꾸고 동기화했습니다.", "info"))
    .catch((e) => showMessage(String(e)))
    .finally(() => refresh().catch(() => {}));
});

/** 개인 채보 창. 연결한 채보 표와, 곡을 찾아 채보를 연결하는 부분. */
async function openPersonalDialog(s) {
  personalServer = s.id;
  $("personal-title").textContent = `개인 채보 · ${s.name}`;
  $("personal-search").value = "";
  $("personal-songs").replaceChildren();
  $("personal-songs-empty").hidden = true;
  $("personal-add").disabled = true;
  await refreshPersonal();
  $("personal-dialog").showModal();
  $("personal-search").focus();
}

async function refreshPersonal() {
  const id = personalServer;
  if (!id) return;
  const charts = await invoke("personal_charts", { id });
  const rows = charts.map((c) => {
    const tr = document.createElement("tr");
    const name = document.createElement("td");
    name.textContent = c.name;
    const song = document.createElement("td");
    song.textContent = c.dir ?? `곡 ${c.song_id} (서버에 없음)`;
    if (!c.dir) song.className = "muted";
    const td = document.createElement("td");
    const remove = document.createElement("button");
    remove.className = "danger";
    remove.textContent = "빼기";
    confirmClick(remove, "한 번 더", () =>
      run(remove, async () => {
        await invoke("remove_personal_chart", { id, songId: c.song_id, name: c.name });
        await refreshPersonal();
      }),
    );
    td.append(remove);
    tr.append(name, song, td);
    return tr;
  });
  $("personal-rows").replaceChildren(...rows);
  $("personal-empty").hidden = charts.length > 0;
}

let searchTimer;
let searchSeq = 0;
$("personal-search").addEventListener("input", () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(searchSongs, SEARCH_DELAY_MS);
});

async function searchSongs() {
  const id = personalServer;
  const query = $("personal-search").value.trim();
  const seq = ++searchSeq;
  $("personal-add").disabled = true;
  const empty = $("personal-songs-empty");
  if (!id || !query) {
    $("personal-songs").replaceChildren();
    empty.hidden = true;
    return;
  }
  let hits;
  try {
    hits = await invoke("search_songs", { id, query });
  } catch (e) {
    showMessage(String(e));
    return;
  }
  // 늦게 온 옛 검색 결과는 버린다.
  if (seq !== searchSeq) return;
  const items = hits.map((h) => {
    const li = document.createElement("li");
    const label = document.createElement("label");
    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "personal-song";
    radio.value = String(h.song_id);
    label.append(radio, ` ${h.dir}`);
    li.append(label);
    return li;
  });
  $("personal-songs").replaceChildren(...items);
  empty.textContent = hits.length
    ? hits.length >= 50
      ? "결과가 많아 앞의 50곡만 보입니다. 더 자세히 검색해 주세요."
      : ""
    : "맞는 곡이 없습니다. 동기화가 끝났는지 확인해 주세요.";
  empty.hidden = !empty.textContent;
}

$("personal-songs").addEventListener("change", () => {
  $("personal-add").disabled = !$("personal-songs").querySelector("input:checked");
});

$("personal-add").addEventListener("click", () => {
  const id = personalServer;
  const picked = $("personal-songs").querySelector("input:checked");
  if (!id || !picked) return;
  const label = picked.parentElement.textContent.trim();
  run($("personal-add"), async () => {
    try {
      const added = await invoke("add_personal_charts", { id, songId: Number(picked.value) });
      if (added.length) {
        showMessage(`${label}에 채보 ${added.length}개를 연결했습니다.`, "info");
      }
    } finally {
      await refreshPersonal();
    }
  });
});

$("personal-dialog").addEventListener("close", () => {
  personalServer = null;
  refresh().catch(() => {});
});

async function refresh() {
  const servers = await invoke("list_servers");
  $("servers").replaceChildren(...servers.map(renderServer));
  $("empty").hidden = servers.length > 0;
  await refreshDrive();
  await refreshCacheLimit();
  await refreshPlayers();
}

async function refreshDrive() {
  const d = await invoke("drive_info");
  const status = $("drive-status");
  const path = $("drive-path");
  if (document.activeElement !== path) path.value = d.mount_point || "";
  if (!d.supported) {
    status.textContent = "이 OS의 가상 드라이브는 아직 준비 중입니다.";
  } else if (d.mounted) {
    status.textContent = `${d.mount_point}에 연결됨`;
  } else {
    status.textContent = `연결되지 않음${d.error ? `: ${d.error}` : ""}`;
  }
}

$("add-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const button = ev.submitter;
  run(button, async () => {
    const url = $("add-url").value.trim();
    const name = $("add-name").value.trim() || null;
    const s = await invoke("add_server", { url, name });
    $("add-url").value = "";
    $("add-name").value = "";
    if (s.insecure) {
      showMessage(`${s.name} 서버를 추가했습니다. ${INSECURE_TEXT}`);
    } else {
      showMessage(`${s.name} 서버를 추가했습니다. 로그인해 주세요.`, "info");
    }
    await refresh();
  });
});

$("drive-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  run(ev.submitter, async () => {
    await invoke("set_mount_point", { path: $("drive-path").value });
    $("drive-path").blur();
    await refreshDrive();
  });
});

async function refreshCacheLimit() {
  const input = $("cache-limit");
  if (document.activeElement !== input) input.value = await invoke("cache_limit_gb");
}

$("cache-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  run(ev.submitter, async () => {
    const gb = Number($("cache-limit").value);
    await invoke("set_cache_limit_gb", { gb });
    $("cache-limit").blur();
    showMessage(`캐시 용량을 ${gb} GB로 바꿨습니다.`, "info");
    await refresh();
  });
});

async function refreshPlayers() {
  const p = await invoke("players");
  $("default-players").textContent = `기본 지원: ${p.defaults.join(", ")}`;
  const items = p.custom.map((path) => {
    const li = document.createElement("li");
    const span = document.createElement("span");
    span.className = "path";
    span.textContent = path;
    const remove = document.createElement("button");
    remove.className = "danger";
    remove.textContent = "빼기";
    confirmClick(remove, "한 번 더 누르면 빼기", () =>
      run(remove, async () => {
        await invoke("remove_custom_player", { path });
        await refreshPlayers();
      }),
    );
    li.append(span, remove);
    return li;
  });
  $("custom-players").replaceChildren(...items);
}

$("add-player").addEventListener("click", () =>
  run($("add-player"), async () => {
    const path = await invoke("add_custom_player");
    if (path) showMessage(`구동기로 추가했습니다: ${path}`, "info");
    await refreshPlayers();
  }),
);

async function showLogPath() {
  const dir = await invoke("log_info");
  $("log-path").textContent = dir
    ? `오류와 다운로드 기록은 이 폴더에 날짜별로 남습니다(최근 7일): ${dir}`
    : "파일 로그를 열지 못해 기록을 남기지 않고 있습니다.";
  $("open-logs").hidden = !dir;
}

$("open-logs").addEventListener("click", () => run($("open-logs"), () => invoke("open_log_dir")));

refresh().catch((e) => showMessage(String(e)));
showLogPath().catch(() => {});
setInterval(() => {
  if (loggingIn.size === 0 && !preServer && !personalServer) refresh().catch(() => {});
}, REFRESH_MS);
