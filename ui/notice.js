// 공지 창. 확인을 누르면 보여준 공지를 다시 띄우지 않는다. 그냥 닫으면 다음에 켤 때 다시 띄운다.
// 서버가 보낸 문자열은 모두 textContent로만 넣는다.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

const LEVELS = { info: "안내", warning: "중요" };

/** 지금 창에 보여준 공지 */
let shown = [];

function render(groups) {
  shown = groups;
  const items = [];
  for (const g of groups) {
    for (const n of g.notices) {
      const li = $("notice-template").content.firstElementChild.cloneNode(true);
      li.querySelector(".title").textContent = n.title;
      const badge = li.querySelector(".badge");
      badge.textContent = LEVELS[n.level] || LEVELS.info;
      badge.classList.add(n.level === "warning" ? "needs_login" : "logged_out");
      li.querySelector(".url").textContent = `${g.server_name} · ${new Date(n.updated_at * 1000).toLocaleString()}`;
      const body = li.querySelector(".body");
      body.textContent = n.body;
      body.hidden = !n.body;
      items.push(li);
    }
  }
  $("notices").replaceChildren(...items);
}

async function load() {
  render(await invoke("pending_notices"));
}

$("ack").addEventListener("click", async () => {
  $("ack").disabled = true;
  try {
    await invoke("ack_notices", { shown });
  } catch (e) {
    const el = $("message");
    el.textContent = `확인 기록을 남기지 못했습니다: ${e}`;
    el.hidden = false;
  } finally {
    $("ack").disabled = false;
  }
});

listen("notices-changed", () => load().catch(() => {}));
load().catch((e) => {
  const el = $("message");
  el.textContent = String(e);
  el.hidden = false;
});
