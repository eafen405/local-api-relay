# 37 — 保留上游字段顺序（API-008）

**What to build:** 目前客户端请求与成功响应经中继转发时会重新序列化 JSON，字段被按字母序重排，破坏了 API-008 的"成功响应 MUST 保留上游状态、对象结构、字段与顺序语义"。本 ticket 让请求与成功响应在转发边界保留上游的字段顺序语义，并新增顺序断言测试。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 请求中的未知/已知字段经中继转发后保持与客户端发送一致的顺序（API-006 pass-through 同时受益）。
- [x] 成功响应字段顺序与上游返回一致，包括发布模型名在客户端边界恢复后的响应（API-008）。
- [x] 新增中继边界测试：使用非字母序字段的请求与响应，断言字段顺序被保留。
- [x] 全套现有测试与新增顺序断言全绿。

Spec coverage: `API-006`, `API-008`.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed (TDD: 顺序断言测试先红后绿)。根因是 `serde_json` 未启用 `preserve_order`：`Value::Object` 默认 `BTreeMap`，任何 parse → 修改 → 再序列化循环都会把键按字母序重排，中继的三条边界（请求转发、非流式成功响应、流式 SSE `data:` 载荷）全部受影响。唯一改动：`Cargo.toml` 的 `serde_json` 依赖加 `features = ["preserve_order"]`（`Map` 换 `IndexMap`，parse 保留插入序、对已存在键的 insert 保持原位、序列化按插入序输出），无任何中继代码改动。新增进程边界测试 `relay_preserves_client_and_upstream_field_order_at_the_process_boundary` + helper `json_object_key_order`（tests/secure_management_surface.rs）：脚本上游响应顶层字段刻意非字母序（choices, zz_extension, object, aa_extension, id, created, model）且携带非发布模型名，断言客户端收到的成功响应键序逐键一致且 `model` 恢复为发布模型名（API-008）；客户端请求含未知字段与嵌套对象且刻意非字母序，断言转发到上游的请求键序逐键一致（API-006）。red 阶段实测失败输出为 `["aa_extension","choices","created","id","model","object","zz_extension"]`（字母序），green 后按上游序输出。已知附带行为变化（非契约破坏）：客户端省略 `stream` 时补插的 `stream:false` 从排序到对象中间改为追加到末尾；管理面响应从字母序变为书写序/声明序，仍完全确定；全部既有测试为 `Value` 相等（IndexMap 键序无关）或子串断言，无整串 JSON 相等断言，故零破坏。全量验证：`cargo check --all-targets` 通过、clippy 零警告、`cargo test` exit 0（secure_management_surface 79 + packaging_lifecycle 27 全绿，106/106）。
- 2026-08-12: Code review (dual-axis, adapted for the git-less repo via the change record `/tmp/ticket37-change-record.md`) completed. **Standards 轴**：通过——无文档化标准违规（repo 无编码标准文件，clippy/rustfmt 由工具强制跳过）；baseline smells 无实质命中（helper `json_object_key_order` 命名诚实、doc 注释准确，新测试是三个顺序断言的唯一共享抽象无 Duplicated Code，`as_object().unwrap()` 在测试断言上下文的 panic 语义可接受）；tautological/implementation-coupled 检查通过（expected 为独立硬编码字面量，red 阶段已实测能区分字母序，测试只接触公开进程边界不触内部实现）；两处附带行为变化（省略 `stream` 时补插默认值追加末尾、管理面响应从字母序改书写序/声明序）判断为可接受 judgement call 且已如实披露。**Spec 轴**：实现满足 API-006/008 核心要求，无缺失、无越界、无 implemented-but-wrong；指出两处测试覆盖盲区（非实现缺陷）——(1) 顺序断言最初只覆盖 chat_completions 非流式，responses 协议与 chat 流式重写路径无顺序断言；(2) 新测试响应 fixture 把 `model` 放在末位，未锁定"原地替换"语义（若 insert 退化为移尾测试仍绿）。两处均已修复（测试文件改动，无实现改动）：响应 fixture 的 `model` 移到中间位置锁定原地替换；`relay_presents_the_published_model_in_chat_sse_chunks` 的上游 chunk 改为非字母序（原 fixture 恰为字母序无法检测重排）并新增对下游 `data:` 载荷键序 `["choices","model","created","object","id"]` 的断言；`relay_access_key_transparently_completes_a_native_responses_call` 新增下游响应键序断言（fixture 本非字母序：id, object, created_at, status, model, output, error, custom_response）与转发请求键序断言（model, input, instructions, tools, reasoning, metadata, client_extension, stream，补插的 `stream` 默认值在末位）。修复后三个顺序断言测试单独运行全绿；`cargo check --all-targets` + clippy 零警告；全套测试 cargo exit 0（secure_management_surface 79 + packaging_lifecycle 27 全绿，106/106，secure 22.95s / packaging 67.79s）。

## Answer

实现完成（TDD：顺序断言先红后绿）。中继转发时 JSON 字段被按字母序重排的问题已修复：请求与成功响应在转发边界保留上游/客户端的字段顺序语义，新增进程边界顺序断言测试并覆盖四条路径（chat 非流式请求+响应、chat 流式重写、responses 非流式请求+响应）。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **根因**：`Cargo.toml` 中 `serde_json` 未启用 `preserve_order`，`Value::Object` 默认是 `BTreeMap`，任何 parse → 修改 → 再序列化循环都会把键按字母序重排。中继的三条边界全部经过该循环：请求转发（`chat_completions`/`responses` handler `from_slice` → `request["model"]=上游名` → `.json(request)`）、非流式成功响应（上游 body → `response["model"]=发布名` → `no_store_json`）、流式 SSE `data:` 载荷（`render_sse_event` parse → 替换 model → `to_string`）。
- **修复（唯一实现改动）**：`Cargo.toml` 的 `serde_json = { version = "1.0", features = ["preserve_order"] }`。`Map` 换为 `IndexMap`：parse 保留插入序（即 wire 上的字段顺序），对已存在键的 `insert` 保持键在原位置，序列化按插入序输出；中继代码零改动。`preserve_order` 使 serde_json 内部引入 `indexmap` 依赖，无新的直接依赖。
- **新增测试**：`relay_preserves_client_and_upstream_field_order_at_the_process_boundary` + helper `json_object_key_order`（tests/secure_management_surface.rs）。脚本上游响应顶层字段刻意非字母序（choices, zz_extension, object, model, aa_extension, id, created）且携带非发布模型名 `scripted-order-model`，断言客户端收到的成功响应键序逐键一致且 `model` 在客户端边界恢复为 `gpt-5.6-sol`、键位保持在中间（API-008）；客户端请求含未知字段与嵌套对象且刻意非字母序，断言转发到上游的请求键序逐键一致（API-006）。red 阶段实测失败输出为 `["aa_extension","choices","created","id","model","object","zz_extension"]`（字母序）。
- **review 后补强的既有测试断言**：chat 流式重写路径（`relay_presents_the_published_model_in_chat_sse_chunks` 改非字母序 chunk + `data:` 载荷键序断言）与 responses 非流式路径（`relay_access_key_transparently_completes_a_native_responses_call` 加下游响应键序 + 转发请求键序断言，含补插 `stream` 在末位）。
- **已知附带行为变化（非契约破坏，已如实记录）**：客户端请求省略 `stream` 时补插的 `stream:false` 从"排序到对象中间"改为"追加到对象末尾"（省略该字段的客户端无位置可保留，追加是唯一自然语义）；管理面响应从字母序变为书写序/声明序（仍完全确定）。全部既有测试为 `Value` 相等（IndexMap `PartialEq` 键序无关）或子串断言，无整串 JSON 相等断言，零破坏。
- **验证**：`cargo check --all-targets` 通过、clippy 零警告；`cargo test` cargo exit 0——`secure_management_surface` 79 个全绿（22.95s，含新增顺序断言测试与补强断言的既有测试），`packaging_lifecycle` 27 个全绿（67.79s），共 106/106。
