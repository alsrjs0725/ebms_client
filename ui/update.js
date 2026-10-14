// 업데이트 창. "업데이트"를 누르면 새 버전을 받아 설치하고 앱을 다시 시작한다.
// 릴리스 내용은 textContent로만 넣는다.
"use strict";

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

function showMessage(text) {
  const el = $("message");
  el.textContent = text;
  el.hidden = false;
}

async function load() {
  const rel = await invoke("update_info");
  if (!rel) {
    await invoke("dismiss_update");
    return;
  }
  $("version").textContent = `v${rel.current} → v${rel.version}`;
  const notes = $("notes");
  notes.textContent = rel.notes;
  notes.hidden = !rel.notes;
}

$("later").addEventListener("click", () => invoke("dismiss_update"));

$("apply").addEventListener("click", async () => {
  $("apply").disabled = true;
  $("later").disabled = true;
  showMessage("새 버전을 받는 중입니다…");
  try {
    // 성공하면 앱이 끝나고 새 버전으로 다시 뜬다.
    await invoke("apply_update");
  } catch (e) {
    showMessage(`업데이트하지 못했습니다: ${e}`);
    $("apply").disabled = false;
    $("later").disabled = false;
  }
});

load().catch((e) => showMessage(String(e)));
