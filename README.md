# wasmer-probe

最小 Rust WASIX 探针：部署到 Wasmer Edge 后验证两件事——
(1) Wasmer 网关是否把 WebSocket 升级转发给 WASM 实例；
(2) WASM 实例能否出站 TCP 直连任意地址。

只做探针，不实现 VLESS，不含任何节点配置、UUID 或密钥。

## 端点（监听 0.0.0.0:$PORT，$PORT 环境变量，默认 80）

| 端点 | 说明 |
|---|---|
| `GET /` | 200，纯文本 `ok` |
| `GET /ws` | WebSocket 升级握手（RFC 6455），握手成功后把收到的文本/二进制帧原样回显；ping 回 pong；握手失败返回 400 说明原因 |
| `GET /dial?host=H&port=P` | 出站 `TcpStream::connect`（5 秒超时），成功后发最小 HTTP GET 并读回首批字节；返回纯文本报告：connect 是否成功、耗时、读到的首行或错误信息。host 只允许域名/IPv4，port 1–65535，否则 400 |

## 本地构建

```sh
# 工具链（只需一次）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
cargo install cargo-wasix
curl https://get.wasmer.io -sSfL | sh   # wasmer CLI

# 编译
cargo wasix build --release
# 产物：target/wasm32-wasmer-wasi/release/wasmer-probe.wasm
```

注意：本机构建时遇到两个环境问题，已解决并记录在案——
- `/tmp` 是 512M tmpfs，曾被旧构建残留占满导致 `cargo install cargo-wasix` 报 `No space left on device`：清掉 `/tmp/cargo-install*` 并用 `TMPDIR=$HOME/tmp-build` 重装解决。
- wasix 工具链的 `lib/rustlib/x86_64-unknown-linux-gnu/bin/` 里缺 `ld`/`ld.lld`，宿主 build script 链接报 `cannot find 'ld'`：在该目录加了 `ld -> /usr/bin/ld` 与 `ld.lld -> rust-lld` 两个符号链接解决。
- 首次下载的 wasmer CLI 二进制损坏（section headers 缺失、段错误）：删掉 `~/.wasmer` 重装后为 7.5.0，可用。

## 本地验证（2026-11-07，用 wasmer 7.5.0 `wasmer run --net`）

```sh
wasmer run --net --env PORT=8080 target/wasm32-wasmer-wasi/release/wasmer-probe.wasm
# 注意：必须用 --env 传 PORT，wasmer run 默认不把宿主环境变量传进 WASM 实例
```

实测结果：

- `GET /` → 200 `ok` ✓
- `GET /ws` → 101 Switching Protocols，发送 `hello-probe` 文本帧，原样回显 ✓（`tmp-build/ws_test.py`，纯标准库客户端）
- `GET /dial?host=example.com&port=80` → 200，报告 `dial: ok`，但**本地结果不可作数**：本机出口经代理拦截，DNS 把任意域名都解析到 198.18.213.x，读回的是代理提示文本而非目标站响应。代码路径（解析→连接→发请求→读回→报告）已验证通，但“能否任意出站 TCP”必须以 Wasmer Edge 上的实测为准。
- `GET /nope` → 404；`port=99999` → 400；`host=bad_host!` → 400 ✓

## 部署到 Wasmer Edge（由主 agent 另起浏览器任务做，本仓库不执行）

方式 A — `wasmer deploy`（需登录 wasmer.io 账号，liangshuangwork）：

```sh
cd wasmer-probe
wasmer deploy
# app.yaml 里 package: . 表示用本地 wasmer.toml 自动发布
```

方式 B — 控制台上传：把 `deploy.zip` 解压后在 Wasmer 控制台用其中的 `wasmer.toml` / `app.yaml` / `.wasm` 部署；或在控制台直接导入 GitHub 仓库（如有）。

部署后从外部验证：

```sh
curl https://<app>.wasmer.app/                                   # 期望 200 ok
curl 'https://<app>.wasmer.app/dial?host=example.com&port=80'     # 看 dial 是否 ok、首行是否为真实 HTTP 响应
# WebSocket：用任意 WS 客户端连 wss://<app>.wasmer.app/ws，发送文本帧，看是否回显
```

判定：
- `/` 通 + `/ws` 回显通 + `/dial` 拿到 example.com 真实首行 → 网关转发 WS 且允许出站 TCP，可继续写 VLESS 服务。
- `/ws` 握手失败/掉线 → 网关不转发 WebSocket，改走 XHTTP packet-up 路线评估。
- `/dial` 连不上/被拒 → Edge 不允许任意出站 TCP，Wasmer 无法做通用代理节点，停下。

## 文件

- `Cargo.toml`、`src/main.rs` — 源码（std + sha1 + base64，无重依赖）
- `wasmer.toml` — package `wasmer-probe`，`[[command]] runner = "wasi"`，`[command.annotations.wasi] env = ["PORT=80"]`
- `app.yaml` — `kind: wasmer.io/App.v0`，`package: .`
- `deploy.zip` — 可部署包：`wasmer.toml`、`app.yaml`、`target/wasm32-wasmer-wasi/release/wasmer-probe.wasm`
