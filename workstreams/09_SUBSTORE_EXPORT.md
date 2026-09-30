# Sub-Store 与发布

## 目标
保持双接入路径 + 原子发布 + 快照回滚（总控 §12）。

## 当前实现审计
- 拉模式：link_substore 为每个 enabled+export 源建 remote sub（URL 带 publish token，**绝不带 admin token**），聚合 collection（零存活源跳过——Sub-Store 对零节点订阅回 500）；prune 只删自己创建且指向本机 host 的对象，keyed on expected 而非成功写入（防 5xx 误删）。
- 推模式：push_exports 每源 upsert `<prefix>-<key>-local`；_prune_local_subs 清理退役源（scoped：-local 后缀 + 本前缀 + source==local）。
- 账本路径：/api/probe/nodes 九字段白名单（substore_bridge.py 单一事实源）+ probe_filter.script.js Script Operator。
- 导出：YAML SafeDumper + YAML1.1 lookalike 强制引号（short-id 123456e2 浮点陷阱，实测真内核 v1.19.29）；链式节点 dialer-proxy 必须在文件内可解析（[前置] 节点直接发布；单前置直指，多前置组；无可解析 dialer 时宁可不发布链式节点）；区域标签 [CC] 以实测出口覆写（幂等正则去旧标签）；tmp+replace 原子写 + meta.json count/updated_at。
- 快照/回滚：现状仅 meta（count/时间），无版本快照与 diff/rollback——总控要求的 export_snapshots 落 R2/R8。

## 待办清单
- [ ] R2：export_snapshots 表
- [ ] R8：Rust 导出 + POST /api/v1/exports/{key}/rollback + diff
- [ ] R8：Script Operator 契约测试（fixtures/expected/）

## 测试证据
- Python 锚点：ExportTest/YamlScalarQuotingTest/DerivedDialerGroupTest/LinkSubstoreTest/PushExportsTest/PruneLocalSubsTest/test_substore_bridge 全文件。

## 下一步
- R2 快照表；R8 导出引擎。
