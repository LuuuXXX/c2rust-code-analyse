# 实施方案：为 Feature::update 添加拷贝控制参数

## 目标概述
在 `Feature::update` 方法中增加一个 `bool` 类型参数，用于控制是否触发拷贝操作。该参数需要与现有的 `link_name` 条件（以 `_c2rust_private_` 开头）**同时满足**时，才会调用 `copy_content_to_other_modules`。同时，在命令行中新增一个参数来控制这个行为。

## 涉及的文件和模块

1. **src/feature.rs**
   - `Feature::update` 方法（第170行）- 修改方法签名
   - `validate_file` 函数（第725行）- 修改签名和拷贝逻辑
   - `copy_content_to_other_modules` 函数（第772行）- 无需修改

2. **src/main.rs**
   - 命令行参数定义（第74行）- 添加新参数
   - 参数解析逻辑（第80-119行）- 解析新参数
   - 调用 Feature::update 的地方（第146行）- 传递新参数

## 技术选型或修改思路

### 1. 参数命名
- **方法参数名**：`enable_copy`（语义：是否启用拷贝功能）
- **命令行参数名**：`--copy-to-modules`（更清晰的表达"拷贝到其他模块"的意图）

### 2. 修改逻辑

#### a) Feature::update 方法
```rust
// 修改前：
pub fn update(&mut self) -> Result<()>

// 修改后：
pub fn update(&mut self, enable_copy: bool) -> Result<()>
```

在调用 `validate_file` 的地方传递参数（第211行）：
```rust
let result = Self::validate_file(&rust_file, &name, &prefixed_name, enable_copy)?;
```

#### b) validate_file 函数
```rust
// 修改前：
fn validate_file(file: &Path, link_name: &str, prefix: &str) -> Result<bool>

// 修改后：
fn validate_file(file: &Path, link_name: &str, prefix: &str, enable_copy: bool) -> Result<bool>
```

**修改拷贝触发逻辑（第763-767行）：**
```rust
// 修改前：
if link_name.starts_with("_c2rust_private_") {
    let _ = Self::copy_content_to_other_modules(file);
}

// 修改后：
// 两个条件同时满足才触发：enable_copy=true 且 link_name 以 _c2rust_private_ 开头
if enable_copy && link_name.starts_with("_c2rust_private_") {
    let _ = Self::copy_content_to_other_modules(file);
}
```

**⚠️ 重要提示**：这个修改会改变现有行为。之前所有 `_c2rust_private_` 开头的文件都会自动触发拷贝，修改后必须显式传递 `--copy-to-modules` 参数才会触发。

#### c) 命令行参数
在 `main.rs` 中添加：
```rust
// 第74行：添加新参数
let opts = hiopt::options!["feature:", "init", "update", "merge", "reinit", "copy-to-modules", "help", "h"];

// 第84行：添加标志变量
let mut copy_to_modules_flag = false;

// 第107-109行：解析新参数
"copy-to-modules" => {
    copy_to_modules_flag = true;
}

// 第146行：传递参数
feature.update(copy_to_modules_flag)?;
```

#### d) 帮助信息更新（第57行）
```rust
println!("  --copy-to-modules    启用拷贝功能，将_c2rust_private_开头的文件内容拷贝到其他模块");
```

## 预期的测试用例

### 测试用例 1：默认行为（不传递新参数）
- **操作**：执行 `code-analyse --feature test_feature --update`
- **预期**：`enable_copy = false`，**不会触发任何拷贝操作**（即使 link_name 以 `_c2rust_private_` 开头）
- **验证**：检查日志，确认没有任何文件被拷贝

### 测试用例 2：启用拷贝功能
- **操作**：执行 `code-analyse --feature test_feature --update --copy-to-modules`
- **预期**：`enable_copy = true`，**只有** `link_name` 以 `_c2rust_private_` 开头的文件才会触发拷贝
- **验证**：检查日志，确认只有满足两个条件的文件被拷贝（enable_copy=true 且 link_name 匹配）

### 测试用例 3：条件组合测试
- **操作**：
  1. 准备两个文件：`fun_c2rust_private_test.rs` 和 `fun_normal.rs`
  2. 执行 `code-analyse --feature test_feature --update --copy-to-modules`
- **预期**：
  - `fun_c2rust_private_test.rs` 会触发拷贝（满足两个条件）
  - `fun_normal.rs` **不会**触发拷贝（link_name 不匹配）
- **验证**：检查日志确认预期行为

### 测试用例 4：拷贝逻辑的正确性
- **操作**：
  1. 创建一个 feature，包含多个模块
  2. 在模块 A 中创建一个 `fun_c2rust_private_xxx.rs` 文件（内容非空）
  3. 在模块 B 中创建同名文件（内容为空），且 C 文件内容相同
  4. 执行 `code-analyse --feature test_feature --update --copy-to-modules`
- **预期**：模块 A 的 `fun_c2rust_private_xxx.rs` 内容被拷贝到模块 B 的同名文件
- **验证**：检查模块 B 的文件内容是否与模块 A 一致

## 行为变更说明

| 场景 | 修改前 | 修改后 |
|------|--------|--------|
| `--update`（不加参数） | `_c2rust_private_*` 自动拷贝 | **不触发拷贝** |
| `--update --copy-to-modules` | - | `_c2rust_private_*` 触发拷贝 |
| 非 `_c2rust_private_*` 文件 | 不拷贝 | 不拷贝 |

## 总结

这个方案通过添加布尔参数，提供了更精确的拷贝控制：
- **显式控制**：必须传递 `--copy-to-modules` 参数才能启用拷贝功能
- **双重条件**：只有 `enable_copy=true` **且** `link_name` 以 `_c2rust_private_` 开头时才触发拷贝
- **安全性**：避免了意外触发拷贝操作