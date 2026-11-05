# 24 — Rust 移植 G-1：canonical 唯一产物模型（指导 §8 / §18-2）

> 目标：把 Python 侧（P4，`mihomo_test/canonical_export.py`）的 canonical
> 模型移植到 `crates/probe-engine`，关闭 `workstreams/21_RUST_PARITY.md` 的
> **G-1**。验收写法沿用 19_ACCEPTANCE_MAP：每条行为一个指名测试。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-engine/src/canonical.rs` | `build_canonical`：多来源去重（指纹）→ 固定排序 → 重命名（只动 name）→ 组拓扑合并 → `render_clash_meta` 渲染 + `CanonicalMeta`（count/digest/content_sha256/renames/source_aliases） |
| `crates/probe-source/src/identity.rs` | 新增 `to_python_json_sorted`（`json.dumps(sort_keys=True, ensure_ascii=False)` 的公开包装，digest 复用同一字节串实现） |
| `crates/probe-engine/src/publish.rs` | `strip_leading_region_tags` 改 `pub(crate)`（canonical 排序键与显示名复用同一剥离语义） |
| `crates/probe-engine/tests/canonical_invariants.rs` | 15 条不变量测试（见 §2） |

## 2. 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试（canonical_invariants.rs） | 状态 |
|---|---|---|---|
| 1 | 同输入 N 轮字节相同（幂等零 PATCH 前提；§18-2/§11.1-2） | `the_same_input_builds_byte_identical_artifacts` | covered |
| 2 | 同指纹不同显示名不分裂：1 条记录 + 别名来源 + renamed 审计 | `a_rename_keeps_one_entry_and_records_the_alias_source` | covered |
| 3 | digest 对改名不敏感、只随连接参数移动（§11.3-4 的镜像） | `the_digest_ignores_renames_and_moves_only_with_connection_changes` | covered |
| 4 | **digest 跨实现同字节串**（Python `_proxy_digest` 直接算出的钉值） | `the_canonical_digest_matches_the_python_implementation`（fd73535266f40185b452a0c01485361f） | covered |
| 5 | 重命名只动 name，其余逐字段不变 | `renaming_touches_only_the_name_field` | covered |
| 6 | 排序抗输入抖动（country→protocol→normalized name→fp） | `sort_order_is_stable_under_shuffled_input` | covered |
| 7 | 冲突后缀唯一且稳定（` #2`/` #3`） | `collision_suffixes_keep_names_unique_and_stable` | covered |
| 8 | 国别回退链：meta → 名称 tag → UN | `an_unset_country_falls_back_to_the_name_tag_then_the_default` | covered |
| 9 | 实测出口国别优先于名称 tag | `a_measured_egress_country_wins_over_the_name_tag` | covered |
| 10 | 关闭重命名仍防同名撞名（enabled=false 语义） | `disabled_rename_still_dedups_colliding_names` | covered |
| 11 | 未知模板占位符回退显示名（Python KeyError 路径） | `unknown_template_placeholders_fall_back_to_the_display_name` | covered |
| 12 | `{{`/`}}` 转义与 `{index}` 支持（str.format 受控子集） | `template_escapes_and_index_are_supported` | covered |
| 13 | 空输入不产空产物（上一稳定产物站住，护栏语义） | `empty_input_builds_nothing_so_the_previous_artifact_stands` | covered |
| 14 | 组拓扑：重名组去重、成员剔除产物缺失项、成员用最终名 | `group_members_missing_from_the_body_are_dropped_and_groups_dedup` | covered |
| 15 | 默认模板/后缀常量与 Python 模块一致 | `defaults_match_the_python_module_constants` | covered |

## 3. 裁决（指导 §17.8）

1. **内容字节不必跨实现一致**：Rust 渲染沿用 `render_clash_meta` 的 JSON-YAML
   （键序归一），Python 是 `yaml.dump`——消费方各自拉各自实现的产物；跨实现
   契约只要求 digest（字节串同）与节点集合等价。与 G-2 的四文件布局对接时，
   以实现各自的 content_sha256 为幂等键。
2. **空结果 = `None`**：`build_canonical` 无可发布节点时不产空产物，调用方
   保留上一稳定版本（与 Python `if not live: return None` 同语义）。
3. **组成员必须引用最终显示名**：与 Python 流程一致（拓扑在重命名之后装配）；
   测试用两态夹具（enabled=false 原名 / enabled=true 最终名）钉住。

## 4. 门禁（2026-11-06 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --locked --workspace` | 29 个测试目标全 ok、**724 passed / 0 failed**（709 基线 + 15 新增） |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |

## 5. 集成边界（后续批次）

本批只交付**模型**（纯函数 + 渲染），不动 engine 的发布链路与 API——接线
（round 产出 → canonical → 存储/Gist）属 G-2 批次（Gist 四文件布局对接
`probe-publish`），届时按 19 号写法补引擎级场景测试。Python 侧行为零改动。
