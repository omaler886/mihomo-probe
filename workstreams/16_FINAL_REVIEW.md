# 最终审查（持续更新至 §21 门禁）

## 目的
按总控 §21 完成定义逐项留证；未达标项如实标注。

## 当前状态：未完成（进行中）
R0/R1 已交付；R2~R12 未开始。逐项门禁现状：
| 门禁 | 状态 | 证据 |
|---|---|---|
| Rust 干净环境构建/启动/健康检查 | 部分达成（本机 cargo test 通过；容器化待 R12） | TEST_REPORT_RUST.md |
| 真 Mihomo 端到端测活 | 未达成（本机无内核；vps 待验） | - |
| 旧数据无损迁移 | 未开始（R2） | - |
| shadow 双跑差异 | 未开始（R10） | - |
| 整轮异常不清空订阅 | Python 侧有护栏与测试锚定；Rust 待 R7 | test_logic |
| Sub-Store 双路径 | Python 侧达成（554 测试含契约）；Rust 待 R8 | test_substore_bridge |
| token 不入 API/日志/指标/历史 | 本机工作区 + 新增内容已由 scanner 验证 | tools/scan_secrets.py |
| Docker Socket 移除/supervisor | 方案 ADR-0002 已定，实施 R12 | workstreams/12 |
| 强制测试与静态检查 | Python 554 通过；cargo fmt/clippy/test 于 R1 起 | 提交记录 |
| 一条命令回滚 | Python 现网回滚=git revert + compose rebuild；Rust 切换后复验 | - |

## 结论
- 不得宣称"迁移完成"。默认实现仍为 Python；Rust 为 shadow。
