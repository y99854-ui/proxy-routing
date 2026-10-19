//! 和代理内核打交道的全部逻辑。支持 mihomo（Clash Verge）和 sing-box（Karing）——两者都提供兼容 Clash 的控制 API。
//!
//! 两条通道：
//! - 控制 API（external-controller，默认 127.0.0.1:9097）：查连接、查策略组、测延迟、切节点。
//!   这条通道必须“直连”，不能再走代理。
//! - 代理端口（mixed-port，默认 7897）：真正去访问用户输入的网址，让 mihomo 按规则分流。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 连接 mihomo 需要的三样东西。前端和后端之间来回传的就是这个结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClashConfig {
    /// 控制 API 地址，如 127.0.0.1:9097
    pub controller: String,
    /// API 密钥，可以为空
    pub secret: String,
    /// 混合代理端口，如 7897
    pub mixed_port: u16,
    /// 配置是从哪个文件读到的（仅用于显示）
    pub source: String,
}

/// 链路上的一个策略组（例如“🚀 节点选择”），以及它可选的节点。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    pub name: String,
    pub kind: String,
    pub now: String,
    pub all: Vec<String>,
    /// 只有 Selector 类型能手动切换；URLTest/Fallback 是自动选的
    pub selectable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitIp {
    pub ip: String,
    pub location: String,
    pub org: String,
    /// 查询出口 IP 这个请求本身走的节点
    pub via_node: String,
}

/// 一次“检查分流”的完整结果。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub url: String,
    pub host: String,
    pub http_status: Option<u16>,
    /// 本工具经代理请求该网址、收到响应头所用的毫秒数
    pub request_ms: u64,
    pub rule: String,
    pub rule_payload: String,
    /// 节点链：第一个是最终出口节点，后面依次是外层策略组
    pub chains: Vec<String>,
    pub node: String,
    pub node_type: String,
    /// mihomo 实际连接的远端地址（通常是节点服务器）
    pub remote_destination: String,
    /// 目标网站被解析到的 IP（走代理时可能为空，由节点远程解析）
    pub destination_ip: String,
    /// mihomo 延迟测试：经该节点访问该网址的往返时间
    pub node_delay_ms: Option<u64>,
    pub node_delay_error: Option<String>,
    pub exit_ip: Option<ExitIp>,
    pub exit_ip_error: Option<String>,
    pub groups: Vec<GroupInfo>,
    pub warnings: Vec<String>,
}

// ───────────────────────── 读取配置 ─────────────────────────

/// 支持的客户端配置文件格式
#[derive(Clone, Copy)]
enum Kind {
    /// Clash Verge Rev 等 mihomo 系客户端：YAML
    Mihomo,
    /// Karing 等 sing-box 系客户端：JSON（experimental.clash_api）
    SingBox,
}

/// 在 dir 下最多 depth 层里查找名为 name 的文件
fn find_file(dir: &Path, name: &str, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                find_file(&p, name, depth - 1, out);
            }
        } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
            out.push(p);
        }
    }
}

/// 所有可能的配置文件。Mac: ~/Library/Application Support/…，Windows: %APPDATA%\…
fn candidate_files() -> Vec<(PathBuf, Kind)> {
    let mut out = Vec::new();

    // Clash Verge Rev：clash-verge.yaml 是 Verge 生成的“运行时配置”，最接近内核实际在用的
    if let Some(base) = dirs::config_dir() {
        let dir = base.join("io.github.clash-verge-rev.clash-verge-rev");
        out.push((dir.join("clash-verge.yaml"), Kind::Mihomo));
        out.push((dir.join("config.yaml"), Kind::Mihomo));
    }

    // Karing：目录名因平台和版本不同，所以在常见根目录下找名字含 karing 的文件夹，再往里搜 service_core.json
    let mut roots: Vec<PathBuf> = [dirs::config_dir(), dirs::data_dir(), dirs::data_local_dir()]
        .into_iter()
        .flatten()
        .collect();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Library/Containers"));
        roots.push(home.join("Library/Group Containers"));
    }
    roots.dedup();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.contains("karing") && e.path().is_dir() {
                let mut found = Vec::new();
                find_file(&e.path(), "service_core.json", 5, &mut found);
                out.extend(found.into_iter().map(|p| (p, Kind::SingBox)));
            }
        }
    }
    out
}

/// external-controller 可能写成 ":9097" 或 "0.0.0.0:9097"，统一成本机地址
fn normalize_controller(raw: &str) -> String {
    let raw = raw.trim().trim_start_matches("http://");
    match raw.rsplit_once(':') {
        Some((host, port)) if host.is_empty() || host == "0.0.0.0" || host == "[::]" || host == "::" => {
            format!("127.0.0.1:{port}")
        }
        _ => raw.to_string(),
    }
}

fn parse_mihomo(path: &Path) -> Option<ClashConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let controller = yaml.get("external-controller")?.as_str()?.to_string();
    if controller.trim().is_empty() {
        return None;
    }
    let secret = yaml.get("secret").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mixed_port = yaml
        .get("mixed-port")
        .or_else(|| yaml.get("port"))
        .and_then(|v| v.as_u64())
        .unwrap_or(7897) as u16;
    Some(ClashConfig {
        controller: normalize_controller(&controller),
        secret,
        mixed_port,
        source: format!("Clash Verge：{}", path.display()),
    })
}

fn parse_singbox(path: &Path) -> Option<ClashConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let api = json.pointer("/experimental/clash_api")?;
    let controller = api.get("external_controller")?.as_str()?.to_string();
    if controller.trim().is_empty() {
        return None;
    }
    let secret = str_of(api, "secret");
    // 代理端口：找第一个 mixed（没有就 http / socks）类型的入站
    let inbounds = json.get("inbounds").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let port_of = |t: &str| {
        inbounds
            .iter()
            .find(|i| str_of(i, "type") == t)
            .and_then(|i| i.get("listen_port"))
            .and_then(|p| p.as_u64())
    };
    let mixed_port = port_of("mixed").or_else(|| port_of("http"))? as u16;
    Some(ClashConfig {
        controller: normalize_controller(&controller),
        secret,
        mixed_port,
        source: format!("Karing：{}", path.display()),
    })
}

/// 命令：自动查找本机代理客户端的配置。
/// 可能同时装了多个客户端，所以逐个试连控制 API，返回第一个能连上的。
#[tauri::command]
pub async fn detect_config() -> Result<ClashConfig, String> {
    let files = candidate_files();
    let mut parsed = Vec::new();
    for (f, kind) in &files {
        let cfg = match kind {
            Kind::Mihomo => parse_mihomo(f),
            Kind::SingBox => parse_singbox(f),
        };
        if let Some(c) = cfg {
            parsed.push(c);
        }
    }
    for c in &parsed {
        if test_controller(c.clone()).await.is_ok() {
            return Ok(c.clone());
        }
    }
    if let Some(c) = parsed.into_iter().next() {
        // 读到了配置但连不上（例如客户端没在运行），仍返回给前端，方便用户看到并修改
        return Ok(c);
    }
    Err(format!(
        "没有找到 Clash Verge 或 Karing 的配置。已查找：\n{}\n\n\
         可以手动填写：Karing 的控制 API 默认是 127.0.0.1:3057，密钥在 service_core.json 里。",
        files
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

// ───────────────────────── 控制 API ─────────────────────────

/// 访问控制 API 的客户端：显式禁用代理，否则请求可能绕回 Clash 自己。
pub(crate) fn api_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

/// 走代理端口的客户端：模拟“普通应用经 Clash 上网”。
fn proxied_client(cfg: &ClashConfig) -> Result<reqwest::Client, String> {
    let proxy = reqwest::Proxy::all(format!("http://127.0.0.1:{}", cfg.mixed_port))
        .map_err(|e| e.to_string())?;
    reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(10))
        // 只看第一跳：分流按域名决定，跟随跳转只会多花时间、多出别的域名的连接
        .redirect(reqwest::redirect::Policy::none())
        // 让连接在请求结束后多留一会儿，方便去 /connections 里找到它
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126 Safari/537.36")
        .build()
        .map_err(|e| e.to_string())
}

pub(crate) fn api_url(cfg: &ClashConfig, path: &str) -> String {
    format!("http://{}{}", cfg.controller, path)
}

pub(crate) fn with_auth(req: reqwest::RequestBuilder, cfg: &ClashConfig) -> reqwest::RequestBuilder {
    if cfg.secret.is_empty() {
        req
    } else {
        req.bearer_auth(&cfg.secret)
    }
}

pub(crate) async fn api_get(cfg: &ClashConfig, path: &str) -> Result<Value, String> {
    let client = api_client()?;
    let resp = with_auth(client.get(api_url(cfg, path)), cfg)
        .send()
        .await
        .map_err(|e| format!("连不上控制 API {}：{e}", cfg.controller))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("控制 API 拒绝访问：密钥（secret）不对".into());
    }
    if !status.is_success() {
        let msg = body.get("message").and_then(|m| m.as_str()).unwrap_or("");
        return Err(format!("控制 API 返回 {status} {msg}"));
    }
    Ok(body)
}

/// 命令：测试控制 API 是否可用，返回内核版本。
#[tauri::command]
pub async fn test_controller(cfg: ClashConfig) -> Result<String, String> {
    let v = api_get(&cfg, "/version").await?;
    let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("未知");
    let meta = v.get("meta").and_then(|x| x.as_bool()).unwrap_or(false);
    Ok(if meta {
        format!("mihomo {version}")
    } else {
        format!("Clash {version}")
    })
}

pub(crate) fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// 当前活动连接里所有指向该主机的连接。
pub(crate) async fn host_connections(cfg: &ClashConfig, host: &str) -> Result<Vec<Value>, String> {
    let data = api_get(cfg, "/connections").await?;
    let conns = data
        .get("connections")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(conns
        .into_iter()
        .filter(|c| {
            let m = &c["metadata"];
            str_of(m, "host").eq_ignore_ascii_case(host) || str_of(m, "destinationIP") == host
        })
        .collect())
}

/// 在当前活动连接里找到目标主机最新的一条，跳过 exclude 里的连接 id（发请求前就已存在的连接）。
async fn find_connection(
    cfg: &ClashConfig,
    host: &str,
    exclude: &HashSet<String>,
) -> Result<Option<Value>, String> {
    Ok(host_connections(cfg, host)
        .await?
        .into_iter()
        .filter(|c| !exclude.contains(&str_of(c, "id")))
        // start 是 RFC3339 时间字符串，按字典序比较就是按时间先后
        .max_by(|a, b| str_of(a, "start").cmp(&str_of(b, "start"))))
}

fn chains_of(conn: &Value) -> Vec<String> {
    conn.get("chains")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// 经代理请求一个 IP 回显服务，并查出这个请求自己走了哪个节点。
/// 返回出口 IP 信息和这个请求的原始节点链（由调用方统一顺序）
async fn query_exit_ip(cfg: &ClashConfig) -> Result<(ExitIp, Vec<String>), String> {
    let client = proxied_client(cfg)?;
    let host = "ipinfo.io";
    let resp = client
        .get("https://ipinfo.io/json")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("查询出口 IP 失败：{e}"))?;
    // 先在连接还活着的时候查链路，再读响应体
    let via_chain = find_connection(cfg, host, &HashSet::new())
        .await
        .ok()
        .flatten()
        .map(|c| chains_of(&c))
        .unwrap_or_default();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("出口 IP 响应无法解析：{e}"))?;
    let location = [str_of(&body, "country"), str_of(&body, "region"), str_of(&body, "city")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    Ok((
        ExitIp {
            ip: str_of(&body, "ip"),
            location,
            org: str_of(&body, "org"),
            via_node: String::new(),
        },
        via_chain,
    ))
}

/// mihomo 的 chains 是“出口节点在前”；sing-box 等内核顺序可能相反。
/// 统一成节点在前：如果第一个是策略组（有 all 列表）而最后一个不是，就反转。
fn normalize_chains(mut chains: Vec<String>, pmap: &Value) -> Vec<String> {
    let is_group = |n: &String| pmap.get(n).and_then(|p| p.get("all")).is_some();
    if chains.len() > 1 && is_group(&chains[0]) && !is_group(&chains[chains.len() - 1]) {
        chains.reverse();
    }
    chains
}

/// 内核自带的延迟测试：让指定节点去访问 url，返回毫秒数。
async fn node_delay(cfg: &ClashConfig, node: &str, url: &str) -> Result<u64, String> {
    let path = format!(
        "/proxies/{}/delay?timeout=5000&url={}",
        urlencoding::encode(node),
        urlencoding::encode(url)
    );
    let v = api_get(cfg, &path).await?;
    v.get("delay")
        .and_then(|d| d.as_u64())
        .ok_or_else(|| "延迟测试没有返回结果".into())
}

fn normalize_url(input: &str) -> Result<url::Url, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("请输入网址".into());
    }
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let u = url::Url::parse(&with_scheme).map_err(|e| format!("网址格式不对：{e}"))?;
    if u.host_str().is_none() {
        return Err("网址里没有域名".into());
    }
    Ok(u)
}

/// 命令：完整检查一个网址的分流情况（含出口 IP）。
#[tauri::command]
pub async fn check_url(cfg: ClashConfig, url: String) -> Result<CheckResult, String> {
    inspect(cfg, url, true).await
}

/// 命令：快速检查，用于批量测试常用网站：跳过出口 IP 查询（它对每个网站都一样，又慢）。
#[tauri::command]
pub async fn quick_check(cfg: ClashConfig, url: String) -> Result<CheckResult, String> {
    inspect(cfg, url, false).await
}

async fn inspect(cfg: ClashConfig, url: String, full: bool) -> Result<CheckResult, String> {
    let target = normalize_url(&url)?;
    let host = target.host_str().unwrap_or("").to_string();
    let target_str = target.to_string();
    let mut warnings = Vec::new();

    // 1. 先记下这个主机已经存在的连接（比如浏览器开着的），避免把它们误认成我们的
    let existing: HashSet<String> = host_connections(&cfg, &host)
        .await?
        .iter()
        .map(|c| str_of(c, "id"))
        .collect();

    // 2. 经代理真实访问一次（HEAD 请求），同时轮询 /connections。
    //    有些网站（如 Reddit、Netflix、Google Scholar）会拒绝非浏览器请求并立刻断开连接，
    //    等请求结束再去查就找不到了；所以在连接一建立、还没被断开时就把它抓下来。
    let client = proxied_client(&cfg)?;
    let done = AtomicBool::new(false);
    let request = async {
        let t0 = Instant::now();
        // HEAD：只要响应头、不下载页面，流量最小，也不像在抓取网页
        let r = client.head(target.clone()).send().await;
        let ms = t0.elapsed().as_millis() as u64;
        done.store(true, Ordering::SeqCst);
        (r, ms)
    };
    let poller = async {
        let mut polls_after_done = 0;
        loop {
            if let Ok(Some(c)) = find_connection(&cfg, &host, &existing).await {
                return Some(c);
            }
            if done.load(Ordering::SeqCst) {
                polls_after_done += 1;
                if polls_after_done > 3 {
                    return None;
                }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    };
    let ((resp, request_ms), mut conn) = tokio::join!(request, poller);

    let http_status = match &resp {
        Ok(r) => Some(r.status().as_u16()),
        Err(e) => {
            let reason = if e.is_timeout() {
                "超时".to_string()
            } else if e.is_connect() {
                "连接失败".to_string()
            } else {
                e.to_string()
            };
            warnings.push(format!(
                "网站没有正常响应（{reason}）。分流信息仍然有效：这只说明经该节点访问这个网站有问题，或网站拒绝了非浏览器的请求。"
            ));
            None
        }
    };
    if let Ok(r) = &resp {
        let code = r.status().as_u16();
        if code == 403 || code == 429 {
            warnings.push(format!(
                "网站返回 HTTP {code}：通常是网站拒绝了自动化请求或该节点 IP，分流信息仍然有效；浏览器里实际能否打开以浏览器为准。"
            ));
        }
    }
    drop(resp);

    // 3. 实在没抓到新连接，就退而用已存在的连接（同一主机通常命中同一条规则）
    if conn.is_none() && !existing.is_empty() {
        conn = find_connection(&cfg, &host, &HashSet::new()).await?;
        if conn.is_some() {
            warnings.push("没抓到本次请求的连接，显示的是该网站已有连接（例如浏览器）的分流信息。".into());
        }
    }

    let Some(conn) = conn else {
        return Err(format!(
            "请求已发出，但在代理内核的活动连接里找不到 {host}。\n\
             请确认代理端口 {} 正确（Karing 用 Rule Based 端口），并且和控制 API 属于同一个客户端。",
            cfg.mixed_port
        ));
    };

    let meta = &conn["metadata"];

    // 3. 读取所有代理/策略组，把链路上每个组的信息取出来
    let proxies = api_get(&cfg, "/proxies").await?;
    let pmap = &proxies["proxies"];

    // mihomo 的 chains 是“出口节点在前”；不同内核/版本顺序可能相反。
    // 统一成节点在前：如果第一个是策略组（有 all 列表）而最后一个不是，就反转。
    let chains = normalize_chains(chains_of(&conn), pmap);
    let node = chains.first().cloned().unwrap_or_default();
    let node_type = str_of(&pmap[&node], "type");
    let groups = chains
        .iter()
        .skip(1)
        .filter_map(|g| {
            let p = pmap.get(g)?;
            let kind = str_of(p, "type");
            Some(GroupInfo {
                name: g.clone(),
                selectable: kind == "Selector",
                kind,
                now: str_of(p, "now"),
                all: p
                    .get("all")
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            })
        })
        .collect();

    // 4. 节点延迟和出口 IP 互不依赖，同时进行（快速模式跳过出口 IP）
    let exit_fut = async {
        if full {
            Some(query_exit_ip(&cfg).await)
        } else {
            None
        }
    };
    let (delay, exit) = tokio::join!(node_delay(&cfg, &node, &target_str), exit_fut);

    let (exit_ip, exit_ip_error) = match exit {
        None => (None, None),
        Some(Ok((mut e, via_chain))) => {
            e.via_node = normalize_chains(via_chain, pmap).into_iter().next().unwrap_or_default();
            if !e.via_node.is_empty() && e.via_node != node {
                warnings.push(format!(
                    "出口 IP 查询走的是「{}」，不是「{node}」，所以下面的出口 IP 不代表本网址的出口。",
                    e.via_node
                ));
            }
            (Some(e), None)
        }
        Some(Err(e)) => (None, Some(e)),
    };
    let (node_delay_ms, node_delay_error) = match delay {
        Ok(d) => (Some(d), None),
        Err(e) => (None, Some(e)),
    };

    Ok(CheckResult {
        url: target_str,
        host,
        http_status,
        request_ms,
        rule: str_of(&conn, "rule"),
        rule_payload: str_of(&conn, "rulePayload"),
        chains,
        node,
        node_type,
        remote_destination: str_of(meta, "remoteDestination"),
        destination_ip: str_of(meta, "destinationIP"),
        node_delay_ms,
        node_delay_error,
        exit_ip,
        exit_ip_error,
        groups,
        warnings,
    })
}

/// 命令：把策略组切换到指定节点。立即生效，影响这个组下的所有网站。
/// Selector 组两种内核都支持；URLTest/Fallback 组 mihomo 支持（固定选择），sing-box 会拒绝。
#[tauri::command]
pub async fn switch_node(cfg: ClashConfig, group: String, node: String) -> Result<(), String> {
    let client = api_client()?;
    let url = api_url(&cfg, &format!("/proxies/{}", urlencoding::encode(&group)));
    let resp = with_auth(client.put(url), &cfg)
        .json(&serde_json::json!({ "name": node }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if text.contains("Selector") {
            return Err(format!(
                "「{group}」是自动选择组，当前内核（sing-box / Karing）不允许手动指定节点。\n\
                 办法：在 Karing 的分流设置里，把用到这个组的规则改成指向一个“手动选择”组，之后就能在这里切换。"
            ));
        }
        Err(format!("切换失败：{status} {text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_singbox_config() {
        let dir = std::env::temp_dir().join("pr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("service_core.json");
        std::fs::write(
            &f,
            r#"{"inbounds":[{"type":"tun"},{"type":"mixed","listen":"127.0.0.1","listen_port":3067}],
               "experimental":{"clash_api":{"external_controller":"127.0.0.1:3057","secret":"abc"}}}"#,
        )
        .unwrap();
        let c = parse_singbox(&f).unwrap();
        assert_eq!(c.controller, "127.0.0.1:3057");
        assert_eq!(c.secret, "abc");
        assert_eq!(c.mixed_port, 3067);
    }

    #[test]
    fn normalizes_chain_order() {
        let pmap: Value = serde_json::json!({
            "节点选择": {"type": "Selector", "all": ["香港01"]},
            "香港01": {"type": "Shadowsocks"}
        });
        let c = normalize_chains(vec!["节点选择".into(), "香港01".into()], &pmap);
        assert_eq!(c[0], "香港01");
        let c = normalize_chains(vec!["香港01".into(), "节点选择".into()], &pmap);
        assert_eq!(c[0], "香港01");
        assert_eq!(normalize_controller(":9097"), "127.0.0.1:9097");
    }
}
