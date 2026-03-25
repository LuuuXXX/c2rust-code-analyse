# 重构方案：File 支持从 JSON 加载并强制校验

## 问题分析

**根本原因：**
`File::new` 每次从 clang AST 重新解析，**不读取已保存的 JSON 文件中的 `git_commit` 状态**。

**代码流程：**
1. `Feature::new` → `get_files()` → `File::new` → `with_c_file` → `load_by_c_file`
2. `load_by_c_file` 从 clang AST 解析，创建全新的 Node，`git_commit = false`
3. `with_c_file` 立即 `save_to`，**覆盖已存在的 JSON 文件**

**这意味着：** 每次调用 `Feature::new` 都会丢失之前保存的 `git_commit` 状态！

## 涉及的文件

| 文件 | 变更 |
|------|------|
| `src/file.rs` | 添加 `loaded_from_json` 标志，修改 `File::new`，添加 `load_from_json` |
| `src/feature.rs` | 修改 `needs_validation` 签名和逻辑，修改 `update` 传参 |

## file.rs 变更

1. File 结构体添加 `loaded_from_json: bool` 字段
2. `File::new` 先尝试从 JSON 加载，失败则从 C 文件解析
3. 添加 `load_from_json` 方法
4. 修改 `with_c_file` 设置 `loaded_from_json: false`
5. 添加 `loaded_from_json()` getter

**注意：** 从 JSON 加载不需要调用 `init_line_info` 和 `init_vars`，因为处理结果已保存在 JSON 中。

## feature.rs 变更

1. `needs_validation` 添加 `loaded_from_json: bool` 参数
2. 如果 `!loaded_from_json`，直接返回 `true`（强制校验）
3. `update` 方法获取 `file.loaded_from_json()` 并传给 `needs_validation`
