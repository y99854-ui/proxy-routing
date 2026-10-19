//! 单站指定：让某个网站固定走指定的节点或策略组（目前仅支持 mihomo / Clash Verge Rev）。
//!
//! 原理（“槽位”）：
//! - 在 Clash Verge Rev 的全局扩展脚本里一次性加入 8 个“槽位”。每个槽位 = 一个规则集文件
//!   (pr-slotN) + 一个隐藏的手动选择组 (PR-指定N)，并把 `RULE-SET,pr-slotN,PR-指定N` 放在所有规则最前面。
//! - 给网站指定目标时：找一个槽位，把该槽位的组切到目标（PUT /proxies），把域名写进槽位的规则文件，
//!   再让内核热重载这个规则集（PUT /providers/rules）。全程不用重启内核，也不会被订阅更新覆盖。
//! - 指向同一目标的网站共用一个槽位，所以最多可以同时指定 8 个不同的目标，网站数量不限。

use crate::clash::{api_client, api_get, api_url, str_of, with_auth, ClashConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const SLOTS: usize = 8;
const GROUP_PREFIX: &str = "PR-指定";
const PROVIDER_PREFIX: &str = "pr-slot";
const DATA_DIR: &str = "proxy-routing";

fn group_name(slot: usize) -> String {
    format!("{GROUP_PREFIX}{slot}")
}
fn provider_name(slot: usize) -> String {
    format!("{PROVIDER_PREFIX}{slot}")
}

/// Clash Verge Rev 的数据目录，也是 mihomo 的工作目录（规则文件的相对路径以它为准）。
fn verge_dir() -> Result<PathBuf, String> {
    dirs::config_dir()
        .map(|d| d.join("io.github.clash-verge-rev.clash-verge-rev"))
        .ok_or_else(|| "找不到系统配置目录".to_string())
}

fn data_dir() -> Result<PathBuf, String> {
    Ok(verge_dir()?.join(DATA_DIR))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    pub domain: String,
    pub target: String,
    pub slot: usize,
}

fn load_assignments() -> Vec<Assignment> {
    data_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("assignments.json")).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_assignments(list: &[Assignment]) -> Result<(), String> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建 {}：{e}", dir.display()))?;
    let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("assignments.json"), text).map_err(|e| e.to_string())
}

/// 把指定列表写成 8 个规则文件（没有网站的槽位写空列表；文件必须存在，否则内核加载配置会失败）。
fn write_slot_files(list: &[Assignment]) -> Result<(), String> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建 {}：{e}", dir.display()))?;
    for slot in 1..=SLOTS {
        let domains: Vec<&str> = list
            .iter()
            .filter(|a| a.slot == slot)
            .map(|a| a.domain.as_str())
            .collect();
        let body = if domains.is_empty() {
            "payload: []\n".to_string()
        } else {
            let mut s = String::from("payload:\n");
            for d in domains {
                s.push_str(&format!("  - DOMAIN-SUFFIX,{d}\n"));
            }
            s
        };
        let path = dir.join(format!("slot{slot}.yaml"));
        std::fs::write(&path, body).map_err(|e| format!("无法写入 {}：{e}", path.display()))?;
    }
    Ok(())
}

fn files_ready() -> bool {
    data_dir()
        .map(|d| (1..=SLOTS).all(|i| d.join(format!("slot{i}.yaml")).exists()))
        .unwrap_or(false)
}

/// 粘贴到 Clash Verge Rev「全局扩展脚本」里的内容。只用 ES5 语法，兼容 Verge 内置的脚本引擎。
pub fn verge_script() -> String {
    format!(
        r#"// ===== 分流检查：单站指定（由「分流检查」工具使用，请勿修改名字）=====
function main(config, profileName) {{
  var SLOTS = {SLOTS};
  var names = [];
  var i;
  var proxies = config["proxies"] || [];
  var groups = config["proxy-groups"] || [];
  for (i = 0; i < groups.length; i++) names.push(groups[i].name);
  for (i = 0; i < proxies.length; i++) names.push(proxies[i].name);
  names.push("DIRECT");
  names.push("REJECT");

  var providers = config["rule-providers"] || {{}};
  var newGroups = [];
  var newRules = [];
  for (i = 1; i <= SLOTS; i++) {{
    var g = "{GROUP_PREFIX}" + i;
    var p = "{PROVIDER_PREFIX}" + i;
    newGroups.push({{
      name: g,
      type: "select",
      hidden: true,
      "include-all-providers": true,
      proxies: names.slice()
    }});
    providers[p] = {{
      type: "file",
      behavior: "classical",
      format: "yaml",
      path: "./{DATA_DIR}/slot" + i + ".yaml"
    }};
    newRules.push("RULE-SET," + p + "," + g);
  }}
  config["proxy-groups"] = newGroups.concat(groups);
  config["rule-providers"] = providers;
  config["rules"] = newRules.concat(config["rules"] || []);
  return config;
}}
"#
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverrideStatus {
    /// "mihomo" / "sing-box" / 其他
    pub core: String,
    pub supported: bool,
    /// 内核里已经有 8 个槽位规则集（说明扩展脚本已生效）
    pub installed: bool,
    pub files_ready: bool,
    pub data_dir: String,
    pub script: String,
    pub assignments: Vec<Assignment>,
    pub message: String,
}

async fn core_name(cfg: &ClashConfig) -> String {
    match api_get(cfg, "/version").await {
        Ok(v) => {
            let ver = str_of(&v, "version");
            if ver.to_lowercase().contains("sing-box") {
                "sing-box".into()
            } else if v.get("meta").and_then(|m| m.as_bool()).unwrap_or(false) {
                "mihomo".into()
            } else {
                "clash".into()
            }
        }
        Err(_) => "unknown".into(),
    }
}

async fn installed_in_core(cfg: &ClashConfig) -> bool {
    match api_get(cfg, "/providers/rules").await {
        Ok(v) => {
            let p = &v["providers"];
            (1..=SLOTS).all(|i| p.get(provider_name(i)).is_some())
        }
        Err(_) => false,
    }
}

async fn api_send(
    cfg: &ClashConfig,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<(), String> {
    let client = api_client()?;
    let mut req = with_auth(client.request(method, api_url(cfg, path)), cfg);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(format!("{path} 返回 {status} {text}"))
    }
}

async fn select_in_slot(cfg: &ClashConfig, slot: usize, target: &str) -> Result<(), String> {
    let path = format!("/proxies/{}", urlencoding::encode(&group_name(slot)));
    api_send(cfg, reqwest::Method::PUT, &path, Some(serde_json::json!({ "name": target })))
        .await
        .map_err(|e| format!("把槽位 {slot} 切到「{target}」失败：{e}"))
}

async fn reload_slot(cfg: &ClashConfig, slot: usize) -> Result<(), String> {
    let path = format!("/providers/rules/{}", provider_name(slot));
    api_send(cfg, reqwest::Method::PUT, &path, None)
        .await
        .map_err(|e| format!("重新加载规则集 {} 失败：{e}", provider_name(slot)))
}

/// 断开某个域名（含子域名）的现有连接，让新规则马上生效。
async fn close_connections(cfg: &ClashConfig, domain: &str) {
    let Ok(data) = api_get(cfg, "/connections").await else { return };
    let empty = vec![];
    let conns = data.get("connections").and_then(|c| c.as_array()).unwrap_or(&empty);
    for c in conns {
        let host = str_of(&c["metadata"], "host").to_lowercase();
        if host == domain || host.ends_with(&format!(".{domain}")) {
            let id = str_of(c, "id");
            let _ = api_send(cfg, reqwest::Method::DELETE, &format!("/connections/{id}"), None).await;
        }
    }
}

/// 内核重启后，如果没保存手动选择，槽位组会回到默认第一项；这里把它们对齐回记录里的目标。
async fn sync_selections(cfg: &ClashConfig, list: &[Assignment]) {
    let Ok(proxies) = api_get(cfg, "/proxies").await else { return };
    let mut done = std::collections::HashSet::new();
    for a in list {
        if !done.insert(a.slot) {
            continue;
        }
        let now = str_of(&proxies["proxies"][group_name(a.slot)], "now");
        if now != a.target {
            let _ = select_in_slot(cfg, a.slot, &a.target).await;
        }
    }
}

/// 从用户输入里取出要指定的域名：去掉协议、路径、端口，转小写。
pub fn normalize_domain(input: &str) -> Result<String, String> {
    let s = input.trim();
    let with_scheme = if s.contains("://") { s.to_string() } else { format!("https://{s}") };
    let u = url::Url::parse(&with_scheme).map_err(|e| format!("网址格式不对：{e}"))?;
    let host = u.host_str().ok_or("网址里没有域名")?.trim_end_matches('.').to_lowercase();
    if host.is_empty() {
        return Err("网址里没有域名".into());
    }
    Ok(host)
}

/// 纯逻辑：在列表里为 domain 安排 target，返回 (新列表, 受影响的槽位)。
pub fn plan_assign(
    list: &[Assignment],
    domain: &str,
    target: &str,
) -> Result<(Vec<Assignment>, Vec<usize>), String> {
    let mut new: Vec<Assignment> = list.iter().filter(|a| a.domain != domain).cloned().collect();
    let mut touched: Vec<usize> = list.iter().filter(|a| a.domain == domain).map(|a| a.slot).collect();
    let slot = if let Some(a) = new.iter().find(|a| a.target == target) {
        a.slot
    } else {
        let used: std::collections::HashSet<usize> = new.iter().map(|a| a.slot).collect();
        (1..=SLOTS).find(|s| !used.contains(s)).ok_or_else(|| {
            format!("已经同时指定了 {SLOTS} 个不同的目标，请先取消一些，或把网站指定到已用过的目标")
        })?
    };
    new.push(Assignment { domain: domain.to_string(), target: target.to_string(), slot });
    touched.push(slot);
    touched.sort();
    touched.dedup();
    Ok((new, touched))
}

// ───────────────────────── 命令 ─────────────────────────

#[tauri::command]
pub async fn override_status(cfg: ClashConfig) -> Result<OverrideStatus, String> {
    let core = core_name(&cfg).await;
    let supported = core == "mihomo";
    let installed = supported && installed_in_core(&cfg).await;
    let assignments = load_assignments();
    if installed {
        sync_selections(&cfg, &assignments).await;
    }
    let message = match core.as_str() {
        "mihomo" if installed => "单站指定已启用".into(),
        "mihomo" => "尚未安装：完成下面的一次性安装后即可使用（安装本身不会改变现有分流）".into(),
        "sing-box" => "当前连接的是 sing-box 内核（如 Karing），它不允许外部添加规则，单站指定只支持 Clash Verge Rev 等 mihomo 内核客户端".into(),
        _ => "当前内核不支持单站指定".into(),
    };
    Ok(OverrideStatus {
        core,
        supported,
        installed,
        files_ready: files_ready(),
        data_dir: data_dir().map(|d| d.display().to_string()).unwrap_or_default(),
        script: verge_script(),
        assignments,
        message,
    })
}

/// 第一步：创建 8 个规则文件（必须在粘贴扩展脚本之前完成，否则内核会因找不到文件而加载失败）。
#[tauri::command]
pub fn override_prepare() -> Result<String, String> {
    let list = load_assignments();
    write_slot_files(&list)?;
    Ok(data_dir()?.display().to_string())
}

/// 可选目标：所有策略组和节点（不含我们自己的槽位组）。
#[derive(Debug, Serialize)]
pub struct Targets {
    pub groups: Vec<String>,
    pub nodes: Vec<String>,
}

#[tauri::command]
pub async fn list_targets(cfg: ClashConfig) -> Result<Targets, String> {
    let v = api_get(&cfg, "/proxies").await?;
    let empty = serde_json::Map::new();
    let map = v["proxies"].as_object().unwrap_or(&empty);
    let mut groups = Vec::new();
    let mut nodes = Vec::new();
    for (name, p) in map {
        if name.starts_with(GROUP_PREFIX) || name == "GLOBAL" {
            continue;
        }
        let kind = str_of(p, "type");
        if matches!(kind.as_str(), "Compatible" | "Pass" | "RejectDrop") {
            continue;
        }
        if p.get("all").is_some() {
            groups.push(name.clone());
        } else {
            nodes.push(name.clone());
        }
    }
    groups.sort();
    nodes.sort();
    Ok(Targets { groups, nodes })
}

/// 命令：把一个或多个域名（如同一网站的主站和 CDN 域名）一起指定到 target。
#[tauri::command]
pub async fn override_set(cfg: ClashConfig, urls: Vec<String>, target: String) -> Result<Vec<Assignment>, String> {
    if !installed_in_core(&cfg).await {
        return Err("单站指定还没安装：请先完成「单站指定」页里的一次性安装".into());
    }
    let mut domains = Vec::new();
    for u in &urls {
        let d = normalize_domain(u)?;
        if !domains.contains(&d) {
            domains.push(d);
        }
    }
    if domains.is_empty() {
        return Err("没有要指定的域名".into());
    }
    let mut list = load_assignments();
    let mut touched = Vec::new();
    for d in &domains {
        let (next, t) = plan_assign(&list, d, &target)?;
        list = next;
        touched.extend(t);
    }
    touched.sort();
    touched.dedup();
    let slot = list.iter().find(|a| a.target == target).map(|a| a.slot).unwrap_or(1);

    // 先切组、再写规则：避免规则先生效时组还指向旧目标
    select_in_slot(&cfg, slot, &target).await?;
    write_slot_files(&list)?;
    for s in touched {
        reload_slot(&cfg, s).await?;
    }
    save_assignments(&list)?;
    for d in &domains {
        close_connections(&cfg, d).await;
    }
    Ok(list)
}

/// 命令：取消一个或多个域名的指定。
#[tauri::command]
pub async fn override_remove(cfg: ClashConfig, domains: Vec<String>) -> Result<Vec<Assignment>, String> {
    let old = load_assignments();
    let mut touched: Vec<usize> = old.iter().filter(|a| domains.contains(&a.domain)).map(|a| a.slot).collect();
    touched.sort();
    touched.dedup();
    let new: Vec<Assignment> = old.into_iter().filter(|a| !domains.contains(&a.domain)).collect();
    write_slot_files(&new)?;
    for s in touched {
        reload_slot(&cfg, s).await?;
    }
    save_assignments(&new)?;
    for d in &domains {
        close_connections(&cfg, d).await;
    }
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(d: &str, t: &str, s: usize) -> Assignment {
        Assignment { domain: d.into(), target: t.into(), slot: s }
    }

    #[test]
    fn same_target_shares_slot() {
        let list = vec![a("a.com", "香港", 1)];
        let (new, touched) = plan_assign(&list, "b.com", "香港").unwrap();
        assert_eq!(new.iter().find(|x| x.domain == "b.com").unwrap().slot, 1);
        assert_eq!(touched, vec![1]);
    }

    #[test]
    fn new_target_takes_free_slot_and_reassign_frees_old() {
        let list = vec![a("a.com", "香港", 1), a("b.com", "日本", 2)];
        let (new, touched) = plan_assign(&list, "a.com", "美国").unwrap();
        let x = new.iter().find(|x| x.domain == "a.com").unwrap();
        assert_eq!(x.slot, 1); // 槽位 1 已空出来，被重新使用
        assert_eq!(touched, vec![1]);
        assert_eq!(new.len(), 2);
    }

    #[test]
    fn slots_exhausted() {
        let list: Vec<Assignment> = (1..=SLOTS).map(|i| a(&format!("s{i}.com"), &format!("t{i}"), i)).collect();
        assert!(plan_assign(&list, "x.com", "new").is_err());
        assert!(plan_assign(&list, "x.com", "t3").is_ok());
    }

    #[test]
    fn domain_normalization() {
        assert_eq!(normalize_domain("https://Scholar.Google.com/scholar?q=1").unwrap(), "scholar.google.com");
        assert_eq!(normalize_domain("youtube.com").unwrap(), "youtube.com");
    }

    #[test]
    fn script_mentions_all_slots() {
        let s = verge_script();
        assert!(s.contains("function main(config"));
        assert!(s.contains("PR-指定"));
        assert!(s.contains("./proxy-routing/slot"));
    }
}


#[cfg(test)]
mod e2e {
    use super::*;
    // 需要先启动 mock（127.0.0.1:19097），并设置 XDG_CONFIG_HOME；默认忽略
    #[test]
    #[ignore]
    fn flow_against_mock() {
        let cfg = ClashConfig { controller: "127.0.0.1:19097".into(), secret: String::new(), mixed_port: 0, source: String::new() };
        tauri::async_runtime::block_on(async {
            let st = override_status(cfg.clone()).await.unwrap();
            println!("status: core={} installed={} files={}", st.core, st.installed, st.files_ready);
            println!("prepare: {}", override_prepare().unwrap());
            let t = list_targets(cfg.clone()).await.unwrap();
            println!("targets: {:?} / {:?}", t.groups, t.nodes);
            let l = override_set(cfg.clone(), vec!["https://scholar.google.com/x".into(), "scholar.googleusercontent.com".into()], "日本01".into()).await.unwrap();
            println!("after set1: {:?}", l);
            let l = override_set(cfg.clone(), vec!["arxiv.org".into()], "日本01".into()).await.unwrap();
            println!("after set2: {:?}", l);
            let l = override_set(cfg.clone(), vec!["scholar.google.com".into()], "节点选择".into()).await.unwrap();
            println!("after reassign: {:?}", l);
            let l = override_remove(cfg.clone(), vec!["arxiv.org".into()]).await.unwrap();
            println!("after remove: {:?}", l);
            let st = override_status(cfg.clone()).await.unwrap();
            println!("status2: installed={} files={} n={}", st.installed, st.files_ready, st.assignments.len());
        });
    }
}
