// 前端逻辑。和 Rust 通信只有一种方式：invoke("命令名", { 参数 })，返回一个 Promise。
// withGlobalTauri: true 让 Tauri 把 API 挂在 window.__TAURI__ 上，所以这里不需要打包工具。
const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const STORE_KEY = "clash-config";
const CUSTOM_KEY = "custom-sites";

let cfg = null;        // 当前连接配置 { controller, secret, mixedPort, source }
let lastUrl = "";      // 最近一次检查的网址，切换节点后用来重新检查

// localStorage 在少数环境下会抛错，统一包一层
const store = {
  get(k) { try { return JSON.parse(localStorage.getItem(k)); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch {} },
};

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function ms(v) {
  if (v == null) return "—";
  const cls = v < 300 ? "good" : v < 800 ? "mid" : "slow";
  return `<span class="${cls}">${v} ms</span>`;
}

// ───────── 视图切换 ─────────

function showView(name) {
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t.dataset.view === name));
  $("view-single").classList.toggle("hidden", name !== "single");
  $("view-batch").classList.toggle("hidden", name !== "batch");
}
document.querySelectorAll(".tab").forEach((t) => (t.onclick = () => showView(t.dataset.view)));

// ───────── 连接设置 ─────────

function fillSettings(c) {
  $("controller").value = c.controller || "";
  $("secret").value = c.secret || "";
  $("mixedPort").value = c.mixedPort || "";
}

function readSettings() {
  return {
    controller: $("controller").value.trim() || "127.0.0.1:3057",
    secret: $("secret").value,
    mixedPort: Number($("mixedPort").value) || 3067,
    source: "手动填写",
  };
}

function setStatus(ok, text) {
  $("statusPill").classList.toggle("ok", ok);
  $("statusPill").classList.toggle("bad", !ok);
  $("statusText").textContent = text;
}

async function connect(c) {
  try {
    const version = await invoke("test_controller", { cfg: c });
    cfg = c;
    store.set(STORE_KEY, c);
    setStatus(true, `${version} · 端口 ${c.mixedPort}`);
    $("settingsMsg").textContent = c.source ? `来源：${c.source}` : "";
    return true;
  } catch (e) {
    cfg = null;
    setStatus(false, "未连接");
    $("settings").classList.remove("hidden");
    $("settingsMsg").textContent = String(e);
    return false;
  }
}

async function autoDetect() {
  try {
    const c = await invoke("detect_config");
    fillSettings(c);
    return await connect(c);
  } catch (e) {
    $("settings").classList.remove("hidden");
    $("settingsMsg").textContent = String(e);
    return false;
  }
}

async function ensureConnected() {
  if (cfg) return true;
  return await autoDetect();
}

$("statusPill").onclick = () => $("settings").classList.toggle("hidden");
$("detectBtn").onclick = autoDetect;
$("connectBtn").onclick = () => connect(readSettings());

// ───────── 网站列表（默认 + 自定义） ─────────

function allSites() {
  const custom = (store.get(CUSTOM_KEY) || []).map((url) => ({ cat: "我的", name: url, url, custom: true }));
  return [...custom, ...DEFAULT_SITES];
}

function fillDatalist() {
  $("siteList").innerHTML = allSites()
    .map((s) => `<option value="${esc(s.url)}">${esc(s.name)}</option>`).join("");
}

// ───────── 单个检查 ─────────

function showError(msg) {
  $("error").textContent = msg;
  $("error").classList.toggle("hidden", !msg);
}

function render(r) {
  $("nodeName").textContent = r.node || "（未知）";
  $("nodeType").textContent = r.nodeType || "";

  // chains 是“出口节点在前”，显示时反过来：从最外层策略组一路到节点
  const path = [...r.chains].reverse();
  $("chain").innerHTML =
    `<span class="chip host">${esc(r.host)}</span>` +
    path.map((p) => `<span class="arrow">→</span><span class="chip">${esc(p)}</span>`).join("");

  $("warnings").innerHTML = r.warnings.map((w) => `<div class="warn">${esc(w)}</div>`).join("");

  $("delay").innerHTML = r.nodeDelayMs != null ? ms(r.nodeDelayMs) : `<span class="slow">失败</span>`;
  $("delay").title = r.nodeDelayError || "";

  $("requestMs").innerHTML = ms(r.requestMs);
  $("httpStatus").textContent = r.httpStatus ? `HTTP ${r.httpStatus}，到收到响应头为止` : "请求失败";

  if (r.exitIp) {
    $("exitIp").textContent = r.exitIp.ip || "—";
    $("exitInfo").textContent = [r.exitIp.location, r.exitIp.org].filter(Boolean).join(" ｜ ");
  } else {
    $("exitIp").textContent = "—";
    $("exitInfo").textContent = r.exitIpError || "";
  }

  $("remote").textContent = r.remoteDestination || "—";
  $("rule").textContent = r.rulePayload ? `${r.rule}  ${r.rulePayload}` : (r.rule || "—");

  renderGroups(r);
  $("result").classList.remove("hidden");
}

const AUTO_KINDS = ["URLTest", "Fallback", "LoadBalance"];

function renderGroups(r) {
  const groups = r.groups;
  if (!groups.length) {
    $("groups").innerHTML = `
      <p class="explain">规则把这个网址直接交给了「${esc(r.node)}」，中间没有经过策略组，所以这里没有可切换的节点。</p>
      <p class="muted small">要改它的走向，需要修改分流规则：在 Karing 的分流设置里调整命中的那条规则，
        或把它指向一个“手动选择”组。</p>`;
    return;
  }

  const hasSelector = groups.some((g) => g.kind === "Selector");
  const tip = hasSelector ? "" : `
    <p class="explain">这条链路上只有自动选择组（${groups.map((g) => esc(g.kind)).join("、")}），节点由内核按测速结果自动挑选。
      Karing（sing-box 内核）不允许通过 API 手动指定这类组的节点，下面的“尝试固定”大多会被拒绝；Clash Verge（mihomo）则支持。</p>
    <p class="muted small">想在这里自由切换：在 Karing 的分流设置里，把命中的规则（${esc(r.rule)}）改成指向一个“手动选择”组。</p>`;

  // 外层组在上，和链路显示顺序一致
  $("groups").innerHTML = tip + [...groups].reverse().map((g) => {
    const auto = AUTO_KINDS.includes(g.kind);
    const label = g.kind === "Selector" ? "切换" : auto ? "尝试固定" : "";
    return `
    <div class="group">
      <div class="group-name">${esc(g.name)} <span class="badge">${esc(g.kind)}</span></div>
      <select data-group="${esc(g.name)}" ${label ? "" : "disabled"}>
        ${g.all.map((n) => `<option ${n === g.now ? "selected" : ""}>${esc(n)}</option>`).join("")}
      </select>
      ${label ? `<button class="switch ${auto ? "ghost" : ""}">${label}</button>` : `<span class="muted small">不支持切换</span>`}
    </div>`;
  }).join("");

  $("groups").querySelectorAll("button.switch").forEach((btn) => {
    btn.onclick = async () => {
      const sel = btn.previousElementSibling;
      btn.disabled = true;
      try {
        await invoke("switch_node", { cfg, group: sel.dataset.group, node: sel.value });
        await runCheck(lastUrl);   // 切完立刻重新检查，看效果
      } catch (e) {
        showError(String(e));
      } finally {
        btn.disabled = false;
      }
    };
  });
}

async function runCheck(url) {
  showError("");
  if (!(await ensureConnected())) {
    showError("还没有连上代理客户端的控制 API，请先在上方设置里连接。");
    return;
  }
  lastUrl = url;
  $("loading").classList.remove("hidden");
  $("checkBtn").disabled = true;
  try {
    render(await invoke("check_url", { cfg, url }));
  } catch (e) {
    showError(String(e));
  } finally {
    $("loading").classList.add("hidden");
    $("checkBtn").disabled = false;
  }
}

$("checkForm").onsubmit = (e) => {
  e.preventDefault();
  runCheck($("urlInput").value);
};

// ───────── 常用网站批量测试 ─────────

function renderBatchRows() {
  $("batchBody").innerHTML = allSites().map((s, i) => `
    <tr data-url="${esc(s.url)}" id="row-${i}">
      <td><div class="site-name">${esc(s.name)}</div><div class="muted small">${esc(s.cat)} · ${esc(s.url)}</div></td>
      <td class="c-node muted">—</td>
      <td class="c-group muted">—</td>
      <td class="c-rule muted small">—</td>
      <td class="c-delay num">—</td>
      <td class="c-act">${s.custom ? `<button class="link remove" data-url="${esc(s.url)}" title="移除">×</button>` : ""}</td>
    </tr>`).join("");

  $("batchBody").querySelectorAll("tr").forEach((tr) => {
    tr.onclick = (e) => {
      if (e.target.closest(".remove")) return;
      showView("single");
      $("urlInput").value = tr.dataset.url;
      runCheck(tr.dataset.url);
    };
  });
  $("batchBody").querySelectorAll(".remove").forEach((b) => {
    b.onclick = () => {
      store.set(CUSTOM_KEY, (store.get(CUSTOM_KEY) || []).filter((u) => u !== b.dataset.url));
      renderBatchRows();
      fillDatalist();
    };
  });
}

function fillRow(tr, r, err) {
  const cell = (cls) => tr.querySelector(cls);
  if (err) {
    cell(".c-node").innerHTML = `<span class="slow">失败</span>`;
    cell(".c-node").title = err;
    cell(".c-rule").textContent = String(err).split("\n")[0];
    return;
  }
  cell(".c-node").textContent = r.node || "—";
  cell(".c-node").classList.remove("muted");
  const g = r.groups[0]; // 节点直接所在的组
  cell(".c-group").innerHTML = g ? `${esc(g.name)} <span class="badge">${esc(g.kind)}</span>` : `<span class="muted">（直接命中）</span>`;
  cell(".c-rule").textContent = r.rulePayload ? `${r.rule} ${r.rulePayload}` : r.rule;
  cell(".c-delay").innerHTML = r.nodeDelayMs != null ? ms(r.nodeDelayMs) : `<span class="slow">超时</span>`;
  // 分流信息拿到了，但网站本身没正常响应（被拒、超时等）：标个提示，鼠标悬停看原因
  if (r.warnings.length) {
    cell(".c-delay").innerHTML += `<div class="warn-tag" title="${esc(r.warnings.join("\n"))}">
      ${r.httpStatus ? "HTTP " + r.httpStatus : "网站未响应"}</div>`;
  }
}

let batchRunning = false;
$("batchRun").onclick = async () => {
  if (batchRunning) return;
  if (!(await ensureConnected())) {
    $("batchProgress").textContent = "还没有连上控制 API，请先在上方设置里连接。";
    return;
  }
  batchRunning = true;
  $("batchRun").disabled = true;
  renderBatchRows();
  const rows = [...$("batchBody").querySelectorAll("tr")];
  let done = 0;
  $("batchProgress").textContent = `0 / ${rows.length}`;

  // 简单的并发池：同时测 4 个，太多会让代理和延迟测试互相干扰
  let next = 0;
  async function worker() {
    while (next < rows.length) {
      const tr = rows[next++];
      tr.classList.add("running");
      try {
        fillRow(tr, await invoke("quick_check", { cfg, url: tr.dataset.url }));
      } catch (e) {
        fillRow(tr, null, String(e));
      }
      tr.classList.remove("running");
      $("batchProgress").textContent = `${++done} / ${rows.length}`;
    }
  }
  await Promise.all([worker(), worker(), worker(), worker()]);
  $("batchProgress").textContent = `完成，共 ${rows.length} 个网站`;
  $("batchRun").disabled = false;
  batchRunning = false;
};

$("addSiteForm").onsubmit = (e) => {
  e.preventDefault();
  const v = $("addSiteInput").value.trim().replace(/^https?:\/\//, "").replace(/\/.*$/, "");
  if (!v) return;
  const list = store.get(CUSTOM_KEY) || [];
  if (!list.includes(v)) list.unshift(v);
  store.set(CUSTOM_KEY, list);
  $("addSiteInput").value = "";
  renderBatchRows();
  fillDatalist();
};

// ───────── 启动：先试上次保存的配置，不行再自动读取 ─────────
fillDatalist();
renderBatchRows();
(async () => {
  const saved = store.get(STORE_KEY);
  if (saved) {
    fillSettings(saved);
    if (await connect(saved)) return;
  }
  await autoDetect();
})();
