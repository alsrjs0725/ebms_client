// 설정 창. 서버 목록·로그인·계정 정보·캐시·가상 드라이브 위치.
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

/** 진행 중인 로그인 (서버 id) */
const loggingIn = new Set();

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

function timeText(unix) {
  return new Date(unix * 1000).toLocaleString();
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

  const badge = li.querySelector(".badge");
  badge.textContent = loggingIn.has(s.id) ? "브라우저에서 로그인 중…" : LOGIN_LABELS[s.login];
  badge.classList.add(s.login);

  const dl = li.querySelector(".account");
  if (s.me) {
    const me = s.me;
    addRow(dl, "계정", me.email ? `${me.display_name} (${me.email})` : me.display_name);
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

  const sync = li.querySelector(".sync");
  if (s.sync.last_error) {
    sync.textContent = `동기화 실패: ${s.sync.last_error}`;
  } else if (s.sync.last_ok_at) {
    sync.textContent = `마지막 동기화: ${timeText(s.sync.last_ok_at)}`;
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

async function refresh() {
  const servers = await invoke("list_servers");
  $("servers").replaceChildren(...servers.map(renderServer));
  $("empty").hidden = servers.length > 0;
  await refreshDrive();
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
    showMessage(`${s.name} 서버를 추가했습니다. 로그인해 주세요.`, "info");
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

confirmClick($("quit"), "한 번 더 누르면 종료 (가상 드라이브도 내려감)", () => invoke("quit"));

refresh().catch((e) => showMessage(String(e)));
setInterval(() => {
  if (loggingIn.size === 0) refresh().catch(() => {});
}, REFRESH_MS);
