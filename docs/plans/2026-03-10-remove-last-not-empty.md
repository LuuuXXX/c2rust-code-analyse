# 重构方案：删除 last_not_empty 字段

## 目标概述

从 `FunctionDecl` 和 `VarDecl` 结构体中删除 `last_not_empty` 字段，因为它与 `git_commit` 功能重复。

## 涉及的文件

| 文件 | 变更 |
|------|------|
| `src/file.rs` | 删除 `last_not_empty` 字段和相关方法 |
| `src/feature.rs` | 修改状态更新逻辑 |

## 当前代码分析

**状态分析：**

| has_committed | not_empty | 动作 |
|---------------|-----------|------|
| false | false | 无变化 |
| false | true | set_git_commit(true), is_updated=true |
| true | false | set_git_commit(false), is_updated=true |
| true | true | 无变化 |

**关键发现：**
- `git_commit` 和 `last_not_empty` 在所有场景下值都相同
- 当 `need_validate=false` 时，`last_not_empty` 就是 `git_commit`
- 两个字段完全冗余

## 重构思路

1. 删除 `FunctionDecl.last_not_empty` 字段
2. 删除 `VarDecl.last_not_empty` 字段
3. 删除 `Kind::get_last_not_empty()` 方法
4. 删除 `Kind::set_last_not_empty()` 方法
5. 修改 `feature.rs` 中的逻辑：
   - 直接使用 `has_committed()` 作为缓存的验证结果
   - 验证后直接更新 `git_commit`
   - 简化状态比较逻辑

## 伪代码

**feature.rs 修改：**
```rust
let not_empty = if need_validate {
    let result = Self::validate_file(&rust_file, &name, &prefixed_name)?;
    Self::update_c_file_mtime(&rust_file)?;
    result
} else {
    node.kind.has_committed()  // 直接用 git_commit 作为上次结果
};

if not_empty {
    translated.insert(Self::normalize_name(&name).to_string(), prefixed_name);
}

let has_committed = node.kind.has_committed();

// 状态变化时更新
if has_committed != not_empty {
    node.kind.set_git_commit(not_empty);
    is_updated = true;
}
```

**file.rs 删除：**
- `FunctionDecl.last_not_empty` 字段
- `VarDecl.last_not_empty` 字段
- `Kind::get_last_not_empty()` 方法
- `Kind::set_last_not_empty()` 方法

## 测试

更新测试中结构体初始化，移除 `last_not_empty` 字段。
