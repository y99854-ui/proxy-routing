# 分流检查

当前版本：**v6702**

一个查看和调整代理分流的小工具（Tauri 桌面应用）。输入网址，就能看到它命中了哪条规则、走的是哪个节点、出口 IP 和延迟，并可以直接切换策略组里选中的节点。

支持 **Karing**（sing-box 内核）和 **Clash Verge Rev**（mihomo 内核）。两者都提供兼容 Clash 的控制 API，本工具通过这个 API 读取和切换。
“单站指定”另需在 Clash Verge Rev 里做一次性安装（见下文），仅支持 mihomo 内核。

## 功能

- **单个检查**：命中的规则、完整节点链、节点延迟、出口 IP、节点服务器地址
- **修改分流**：切换链路上“手动选择”（Selector）组的节点，切换后自动重新检查
- **单站指定**：让某个网站（连同它的 CDN 等相关域名）固定走指定的节点或策略组，不影响同组其他网站（仅 Clash Verge Rev）
- **常用网站**：一键批量测试 AI、海外常用、影音、科研、国内等常用网站的分流情况；可添加自己的网站
- 输入框下拉推荐常用网站；自动读取 Karing / Clash Verge 的连接配置；深色模式

## 下载使用

在 [Releases](../../releases) 下载对应的安装包：

| 系统 | 文件 |
|---|---|
| Mac（Intel 和 Apple 芯片通用） | `proxy-routing_版本号_universal.dmg` |
| Windows | `proxy-routing_版本号_x64-setup.exe`（推荐）或 `.msi` |

Mac：打开 `.dmg`，把「分流检查」拖进「应用程序」。应用没有苹果开发者签名，第一次打开会被系统拦截：

- 提示“无法验证开发者”：到「系统设置 → 隐私与安全性」点「仍要打开」
- 提示“已损坏”：在终端执行 `xattr -dr com.apple.quarantine /Applications/分流检查.app`

Windows：运行安装程序即可；如出现 SmartScreen 提示，点「更多信息 → 仍要运行」。

安装包由 GitHub Actions 自动打包：推送 `v` 开头的标签即会生成并附到对应发布页。

## 连接设置

启动时会自动查找客户端配置。找不到时手动填写：

| 客户端 | 控制 API | 代理端口 | 密钥 |
|---|---|---|---|
| Karing | `127.0.0.1:3057`（Control and Sync） | `3067`（Rule Based） | 「设置 → 在线面板」网址里 `secret=` 后面的值 |
| Clash Verge Rev | 设置里「外部控制」的地址 | mixed-port（默认 7897） | 「外部控制」里的密钥 |

Karing 的代理端口一定要用 Rule Based（按规则分流）那个，Direct All / Proxy All 不走规则，查出来的结果不代表平时的分流。

## 单站指定（仅 Clash Verge Rev / mihomo）

让某个网站固定走指定的节点或策略组，不影响同组的其他网站，立即生效，订阅更新后也不会丢。

一次性设置（在「单站指定」页里有引导）：

1. 点「准备文件」，工具会在 Clash Verge Rev 的数据目录下创建 `proxy-routing/` 文件夹和 8 个规则文件；
2. 点「复制脚本」，到 Clash Verge Rev「订阅」页的「全局扩展脚本」里粘贴并保存（原来写过自定义脚本的需要手动合并）；
3. 回到工具点「检查是否生效」。

之后在单个检查的结果里，或「单站指定」页里，选好目标点「指定」即可。

原理：扩展脚本加入 8 个“槽位”，每个槽位是一个规则集文件加一个隐藏的手动选择组，并放在所有规则最前面。
指定网站时，工具把域名写进某个槽位的规则文件、把槽位组切到目标，再让内核热重载规则集。
指向同一目标的网站共用一个槽位，所以最多同时指定 8 个不同目标，网站数量不限。

Karing（sing-box 内核）不允许外部添加规则，暂不支持单站指定。

## 已知限制

- Karing（sing-box）只允许通过 API 切换“手动选择”组。规则直接指向自动测速组（URLTest）或直连时，无法在工具里切换，需要在 Karing 的分流设置里把规则改为指向一个手动选择组。Clash Verge（mihomo）也支持固定自动测速组的节点。
- 浏览器在 fake-ip / TUN 模式下的匹配可能和本工具略有不同（尤其是 IP 类、进程类规则）。
- 检测用的是 HEAD 请求，只取响应头。少数网站会拒绝自动化请求（显示 HTTP 403 或“网站未响应”），这不影响分流结果的准确性。

## 从源码运行

需要 Xcode 命令行工具、[Rust](https://rustup.rs) 和 [Node.js](https://nodejs.org)。

```bash
npm install
npm run dev      # 开发模式
npm run build    # 打包，产物在 src-tauri/target/release/bundle/
```

打一个 Intel 和 Apple 芯片都能用的版本：

```bash
rustup target add x86_64-apple-darwin
npm run tauri build -- --target universal-apple-darwin
```

## 项目结构

```
src/                 前端（普通网页）
  index.html         界面
  style.css          样式
  main.js            交互逻辑，通过 invoke() 调用 Rust
  sites.js           常用网站列表
src-tauri/           后端（Rust）
  tauri.conf.json    Tauri 配置
  src/main.rs        入口，注册命令
  src/clash.rs       和代理内核 API 通信的全部逻辑
```

## 工作原理

1. 经代理端口对目标网址发一次 HEAD 请求，让内核按规则分流；
2. 请求进行中轮询控制 API 的 `/connections`，抓到这条连接的 `rule` 和 `chains`；
3. `GET /proxies/{节点}/delay` 测该节点访问此网址的延迟；
4. 经代理请求 ipinfo.io 得到出口 IP，并核对它是否走了同一个节点。

切换节点调用 `PUT /proxies/{策略组}`。
