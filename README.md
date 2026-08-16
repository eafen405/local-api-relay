# Local API Relay

本地 API 中转（local-api-relay）：单进程 Rust 服务，把多个上游供应商聚合成一个 OpenAI 兼容端点，按成本与可用性自动路由。loopback-only，个人自用。

## 部署指南（给 hermes）

> 目标：在 Linux x86_64 服务器上完成安装、初始化、配置与验证。全部命令无需 root，每一步都给出成功判据。
>
> **本仓库不携带任何开发机的运行配置或密钥。** 请在服务器上用**你自己的上游供应商（base_url + api_key）**按第 4 步接入你自己的模型；DSH 接入模板见 `examples/dsh-settings.example.yaml`。

### 0. 前置条件

- Linux x86_64（glibc），空闲内存 ≥ 256 MB（实测常驻约 15 MB）
- 无其他运行时依赖：SQLite 静态内嵌，管理前端内嵌在二进制里
- 服务只监听 `127.0.0.1:8787`——在服务器本机用 curl 操作即可，远程访问走 SSH 隧道或 Tailscale

### 1. 安装（推荐：GitHub Release tarball）

```bash
cd /opt   # 任意目录
curl -LO https://github.com/eafen405/local-api-relay/releases/download/v0.1.0/local-api-relay-0.1.0.tar.gz
mkdir -p local-api-relay-0.1.0 && tar -xzf local-api-relay-0.1.0.tar.gz -C local-api-relay-0.1.0
cd local-api-relay-0.1.0
bash install.sh
```

判据：无报错，且 `~/.local/bin/local-api-relay` 与 `~/.local/bin/local-api-relay-service` 存在。安装是幂等的，重复执行是安全 no-op；安装新版本 tarball 会自动执行升级流程。

### 2. 初始化管理员并启动

```bash
~/.local/bin/local-api-relay init-admin
# 输出：Administrator bootstrap credential: <一次性凭据>（只打印这一次，记下来）
~/.local/bin/local-api-relay-service start     # 判据：exit 0，等待 ready
curl -s http://127.0.0.1:8787/ready            # 判据：{"status":"ready"}
```

### 3. 管理员登录并强制换密码（bootstrap 凭据必须更换）

```bash
CJ=/tmp/relay-admin.cookies
curl -s -c $CJ -X POST http://127.0.0.1:8787/admin/login \
  -H 'Content-Type: application/json' -d '{"password":"<bootstrap 凭据>"}'
curl -s -b $CJ -X POST http://127.0.0.1:8787/admin/change-password \
  -H 'Content-Type: application/json' -d '{"new_password":"<新管理员密码>"}'
```

判据：两次响应都没有 `error` 字段。此后所有 `/admin/*` 请求都带 `-b $CJ`。管理界面是 `http://127.0.0.1:8787`（登录后可视化操作，以下 API 用于脚本化）。

### 4. 配置三步曲 + 发访问密钥

**4.1 上游供应商**（每把上游 key 建一个供应商；返回的 `id` 记为 `PROVIDER_ID`）：

```bash
curl -s -b $CJ -X POST http://127.0.0.1:8787/admin/providers \
  -H 'Content-Type: application/json' \
  -d '{"display_name":"My-Provider","base_url":"https://upstream.example/v1","api_key":"sk-..."}'
```

**4.2 发布模型**（对客户端可见的模型名 + 计费价，单位 RMB/百万 token；id 就是 name）：

```bash
curl -s -b $CJ -X POST http://127.0.0.1:8787/admin/published-models \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-model","input_price_rmb":"1.0","output_price_rmb":"2.0","cached_input_price_rmb":"0.1"}'
```

**4.3 模型路由**（发布模型 × 供应商 × 上游模型名 × 协议 × 成本倍率；创建时立即发原生探测并返回健康）：

```bash
curl -s -b $CJ -X POST http://127.0.0.1:8787/admin/model-routes \
  -H 'Content-Type: application/json' \
  -d '{"published_model_id":"my-model","provider_id":"<PROVIDER_ID>","upstream_model_name":"<上游模型名>","protocol":"chat_completions","cost_multiplier":"1"}'
# 返回：{"id":"<ROUTE_ID>","health":"available"|"unavailable"}
```

- 协议分流：`chat_completions` 路由只服务 `/v1/chat/completions`，`responses` 路由只服务 `/v1/responses`，互不相通；同一发布模型可同时有两条不同协议的路由。
- 路由选择：健康路由中最便宜（倍率最低）优先；连续失败（默认 2 次）自动隔离并指数退避恢复探测；available 路由每 10 分钟轻验证一次（模型仍在上游目录就不打扰）。

**4.4 中转访问密钥**（客户端 Bearer；返回的 `secret` 字段就是完整密钥）：

```bash
curl -s -b $CJ -X POST http://127.0.0.1:8787/admin/relay-access-keys \
  -H 'Content-Type: application/json' \
  -d '{"label":"main","model_route_ids":["<ROUTE_ID>"]}'
# 返回：{"id":...,"secret":"lar_...",...} —— 记录 secret
```

新建的路由不会自动授权给已有密钥，需要在密钥上显式勾选（`PATCH /admin/relay-access-keys/:key_id`，body 同创建）。

### 5. 验证中转

```bash
KEY='lar_...'
curl -s http://127.0.0.1:8787/v1/models -H "Authorization: Bearer $KEY"
curl -s http://127.0.0.1:8787/v1/chat/completions -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"my-model","messages":[{"role":"user","content":"hi"}],"max_tokens":16}'
```

判据：`/v1/models` 只列出「该密钥有可用路由」的发布模型；completions 返回 200 + choices。健康面板：`GET /admin/operations`（带 cookie）或管理界面「运维」。

### 6. 运维

```bash
~/.local/bin/local-api-relay-service status    # exit 0 运行中 / 3 已停止
~/.local/bin/local-api-relay-service restart   # 有界优雅重启（等 30s 收尾）
~/.local/bin/local-api-relay backup --reason manual      # 手工快照
~/.local/bin/local-api-relay restore <备份名>              # 文件级恢复
~/.local/bin/local-api-relay-service rollback            # 回滚上次升级
```

目录（XDG，全部 owner-only 700）：data `~/.local/share/local-api-relay/`（SQLite + backups 快照，定期清理旧快照）、config `~/.config/local-api-relay/`（`service.json` 可改 `"port"`）、state `~/.local/state/local-api-relay/`（日志：14 天轮转、上限 200MiB）。Windows 原生版使用 `%LOCALAPPDATA%\local-api-relay`（数据与状态）和 `%APPDATA%\local-api-relay`（配置），目录位于当前用户配置文件内，保持仅当前用户可访问。

升级：下载新版本 tarball → 解压 → `bash install.sh`（自动 preflight + 迁移备份 + 切换 + 重启；失败可用 `rollback` 还原）。

### 7. 资源占用（实测）

含多路由中转流量的实测：RSS 常驻约 15MB、峰值不变、无 Swap、CPU 约 0.1%。可变项只有请求体与响应体的整包缓冲（内存 ≈ 并发数 × 单次请求体/响应体），请求体上限 16MiB。systemd 用户单元建议 `MemoryMax=256M` 兜底。

### 8. 接入客户端 / DSH

任意 OpenAI 兼容客户端：base_url `http://127.0.0.1:8787/v1`，API key = `lar_...`。

DSH（DeepSeek Harness）：`$DSH_HOME/settings.yaml` 热加载，无需重启：

```yaml
agent-default-model:
  provider: local-deepseek
  model: deepseek-v4-pro
  reasoningEffort: max
llm-pi-ai:
  providers:
    local-deepseek:
      displayName: local-deepseek
      apiKeyEnv: LOCAL_DEEPSEEK_API_KEY
      api: openai-completions
      baseURL: http://127.0.0.1:8787/v1
      compat:
        thinkingFormat: deepseek
      models:
        - id: deepseek-v4-flash
        - id: deepseek-v4-pro
    local-gpt:
      displayName: local-gpt
      apiKeyEnv: LOCAL_GPT_API_KEY
      api: openai-responses
      baseURL: http://127.0.0.1:8787/v1
      models:
        - id: gpt-5.6-sol
```

`apiKeyEnv` 对应的变量名写在 `$DSH_HOME/.credentials.yaml`（`LOCAL_DEEPSEEK_API_KEY: lar_...`）。子代理固定模型写在 preset 的 `agent.cordis.yml`：

```yaml
agentOptions:
  provider: local-deepseek
  model: deepseek-v4-flash
```

注意：chat 协议的中转路由才服务 chat_completions；gpt 若只有 responses 路由，DSH 侧要用 `api: openai-responses`。

### 9. 安全边界（个人自用设计，勿用于多用户环境）

- 只绑 127.0.0.1、无 TLS；不要直接暴露公网，远程请走 SSH 隧道 / Tailscale
- 上游 key 与中转 key 明文存于本机 owner-only 目录，且界面可再显示（有意为之的取舍）
- 管理员凭据与中转密钥完全隔离；bootstrap 凭据只打印一次，不进入脚本、环境变量或日志

### 源码构建（备选）

```bash
cargo build --release                # 需要 Rust 工具链（edition 2024）
packaging/build-archive.sh           # 产出 dist/local-api-relay-<version>.tar.gz
```

---

# Local API Relay（原文文档）

一个只监听本机 loopback 的本地 API 中转。它为客户端发布模型，并在多个上游 API 供应商之间按成本与可用性路由请求；它不管理 Codex、KimiCode 或其他客户端工具的配置。

## First Run

The first implementation slice provides the secure local management surface:

```bash
cargo run -- init-admin
cargo run -- serve
```

`init-admin` prints a one-time administrator bootstrap credential. Sign in at `http://127.0.0.1:8787`, then replace that credential before Operations becomes available. The management session is separate from relay access keys, and no relay access key has management permissions.

The service uses `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `XDG_STATE_HOME` (or their standard home-directory fallbacks) under `local-api-relay`. On Windows it uses `%LOCALAPPDATA%\local-api-relay` for data/state and `%APPDATA%\local-api-relay` for configuration. It always binds `127.0.0.1`; use `cargo run -- serve --port <port>` for an explicit alternate loopback port.

## Relay Calls

After an administrator has configured an available Chat Completions model route, create a relay access key from Operations and select the model routes it may use. The full key is shown only at creation time; it is separate from the administrator credential and cannot access `/admin/*`.

Clients use the key as a standard Bearer credential:

```bash
curl http://127.0.0.1:8787/v1/models \
  -H 'Authorization: Bearer <relay-access-key>'

curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Authorization: Bearer <relay-access-key>' \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"Hello"}]}'
```

The relay only exposes published models for which the key has a currently available Chat Completions model route. It replaces the published model and client key only at the upstream boundary, preserves unknown request and response fields, and restores the published model in successful responses.

Inbound request bodies are limited to 16 MiB (16,777,216 bytes), comfortably covering multi-turn harness conversations, tool-call payloads, and 1M-context sessions; a larger body is rejected immediately with `413` and never reaches upstream routing (API-016).

## Verification

```bash
cargo test --test secure_management_surface
cargo test --test packaging_lifecycle
```

The integration test starts the compiled service with isolated XDG paths and covers initialization, one-time bootstrap output, forced credential replacement, session protection, relay-key isolation, private local state, ready behavior, and failed startup paths. The packaging test drives the real installer and lifecycle scripts at the process boundary and covers the versioned layout, stable entry, owner-only permissions, lifecycle commands, ports, bounded graceful stop, and bootstrap-credential secrecy.

## Packaging and Lifecycle

The production artifact is a self-contained, versioned Linux x86_64 archive:

```bash
packaging/build-archive.sh        # produces dist/local-api-relay-<version>.tar.gz
tar -xzf dist/local-api-relay-<version>.tar.gz
./install.sh                      # idempotent, user-level, no root required
```

Installing lays out versioned program files side by side under
`~/.local/opt/local-api-relay/<version>/`, selects the current version through
the stable user-level entry `~/.local/bin/local-api-relay`, installs the
lifecycle script at `~/.local/bin/local-api-relay-service`, and keeps the SQLite
database, process configuration, and runtime state under the XDG data, config,
and state directories with the `local-api-relay` application name. Every
directory and secret-bearing file is owner-only, and the management frontend is
embedded in the binary — there is no separate frontend directory or runtime.

The service always binds `127.0.0.1`; the default port is `8787`, and
`~/.config/local-api-relay/service.json` (or `serve --port <port>`) selects
another stable port. The process never scans for or silently switches to a free
port.

Lifecycle commands are fixed and scriptable (exit 0 running / 3 stopped):

```bash
~/.local/bin/local-api-relay-service start     # idempotent; waits for ready
~/.local/bin/local-api-relay-service status    # running / starting / stopped
~/.local/bin/local-api-relay-service restart   # bounded graceful restart
~/.local/bin/local-api-relay-service stop      # bounded graceful stop
~/.local/bin/local-api-relay-service rollback  # reverse the last upgrade (PKG-014)
```

The launcher captures the serve process's structured stderr into the state log
directory and rotates the captured file at the earlier of 20 MiB or the
calendar-day boundary, keeping nothing older than 14 days and capping the set at
200 MiB; the service itself mirrors every event into a live-managed rotating log
in the same directory. `stop`/`restart` stop accepting new calls, wait at most
30 seconds for in-flight calls to finish, and then cancel the rest and exit
cleanly.

## Upgrades and Rollback

Installing a newer archive performs the upgrade flow (PKG-013). It keeps the
previous program version installed side by side, verifies the new binary before
anything switches (it must start, read the process configuration, serve the
embedded management page, and bind the configured port against a staged copy of
the database), creates and verifies the migration pre-backup when a forward
schema migration is required, atomically switches the stable entry, and
restarts the running service or Windows login task — the client address and the
management entry never change. A failure at any pre-switch stage leaves the
stable entry and the live database untouched and restores the previous service.

When the upgrade fails after the switch, `local-api-relay-service rollback`
reverses it (PKG-014): it stops the service, switches the stable entry back to
the previous version, and — when the forward migration already committed —
explicitly restores the migration pre-backup with the previous binary. The live
database is never downgraded in place; a failed upgrade that never committed a
migration needs no restore because the database was left untouched. Each
upgrade records its own previous version, so a rollback always returns to the
version that was serving before it.

The one-time administrator bootstrap credential is printed only by
`local-api-relay init-admin` and never enters scripts, the process environment,
or any log.
