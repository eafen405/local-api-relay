# 43 — 界定 SSE 重写路径的无损转发语义（ROUTE-013）

**What to build:** 流式响应在上游模型名与发布模型名不一致时会被重编码（解析 → 改写 model 字段 → 重序列化 → 重建为单行 `data:`），与 ROUTE-013「成功首事件必须无损转发」的关系未界定。本 ticket 裁决「无损」的精确含义：语义无损（字段全保留、顺序保留）还是字节保真（逐字节透传）。若字节保真为必要，则改为在原始字节上原地改写 model 字段，不整体重序列化；若语义无损即可，则补测试钉死当前行为的边界，确保已知风险（超大整数/高精度浮点 round-trip、unicode 转义规范化、多行 data 折叠）被明确定义为可接受并记录理由。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 裁决「无损转发」= 语义无损还是字节保真，结论记录为 spec 解释（决策追溯表新增一行）。
- [ ] 若需字节保真：重写路径改为不整体重序列化，仅改写 model 字段；补进程边界测试断言多行 data、未知字段、字段顺序、数字精度的保持。
- [x] 若语义无损即可：补测试把已知 round-trip 边界钉为可接受行为，并在 spec 或 ticket 中记录理由。
- [x] 全套现有测试保持绿，clippy 零警告。

Spec coverage: `ROUTE-013`, `API-006`, `API-008`, 用户故事 25/26.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. 裁决：「无损」= 语义无损（非字节保真）——需要模型替换的场景字节保真定义上不可能（API-008 要求呈现发布模型名）；无需替换时已逐字节透传。ROUTE-013 增补「无损 = 语义无损」界定；验收矩阵 ROUTE-012–ROUTE-015 行 + 决策追溯表一行；实现代码零改动。新增进程边界测试 `relay_rewrites_only_the_model_field_and_pins_semantic_losslessness`（Chat + Responses 双路径，钉住多行 data 折叠、u64::MAX/2^53+1 精确往返、unicode 规范化、超高精度浮点 f64 最短往返边界、未知字段与字段顺序保留）。
- 2026-08-12: Code review (dual-axis) 通过——Standards 轴无硬性违规（baseline smells 无实质命中；Chat/Responses 两半测试形状重复系刻意覆盖不同路径）；Spec 轴无缺失实现、无越界，两处已修复——补 Responses `response` 对象字段顺序断言（原只钉 Chat 字段顺序），折叠断言强化为「事件区仅一条 `data:` 行（排除 `[DONE]`）」；ROUTE-013 规范句增补「`i64`/`u64` 精确、`f64` 最短往返；超出可表示范围的数值四舍五入到最近可表示值，属已记录的可接受边界」，消除与 ticket 舍入边界记录的歧义。验证：全套 cargo exit 0、111 测试全绿、clippy 零警告。

## Answer

裁决：「无损转发」= **语义无损**（semantic losslessness），非字节保真。依据：

- 需要重写的场景（上游模型名 ≠ 发布模型名）下字节保真定义上不可能：API-008 要求客户端边界呈现发布模型名，字节必然改变。因此「无损」只能指事件语义内容完全保留。
- 重写路径已保留对象结构、全部已知及未知字段与字段顺序（ticket 37 启用 `serde_json` 的 `preserve_order`，`model` 原地替换后按插入序序列化）；无需替换时 `render_sse_event` 直接逐字节透传原始事件（`event.raw`）。
- 已知 round-trip 边界的定性（均可接受并有理由）：
  1. **多行 `data:` 折叠**：SSE 规范定义多条 data 行以 `\n` 连接，折叠为单行解码后同一事件，语义等价。
  2. **unicode 转义规范化**：`\uXXXX` 解码为实际字符，JSON 字符串值不变，语义等价。
  3. **数值往返**：`i64`/`u64` 精确，f64 以最短往返表示精确往返；字面精度超出 `serde_json` 可表示范围的数四舍五入到最近可表示值——LLM API 的数值字段（token 数、索引、时间戳、分数）均在精确范围内，超出 f64 范围的数在事件验证阶段即被判为非法上游响应，不会被静默改写。

落地：

- spec 解释：ROUTE-013 增补「无损 = 语义无损」界定（一致时逐字节透传；替换时保留结构/字段/顺序、仅替换 `model` 值、编码变换限语义等价形式、数值在可表示范围内精确往返）；验收矩阵 `ROUTE-012`–`ROUTE-015` 行同步修订；决策追溯表新增一行指向本 ticket。
- 进程边界测试 `relay_rewrites_only_the_model_field_and_pins_semantic_losslessness` 钉住 Chat 与 Responses 两条重写路径：多行 data 折叠为单行、u64::MAX 与 2^53+1 整数精确往返、unicode 转义值保留、超高精度浮点钉为 f64 最短往返、未知字段与字段顺序保留、`response.model` 嵌套重写、事件名与类型化终止保留。
- 全套测试绿，clippy 零警告。
