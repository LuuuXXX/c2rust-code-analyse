# 实施方案：在 merge 后的 lib.rs 中添加模块别名

## 目标概述
在 `Feature::merge` 后生成的 `lib.rs` 中，为每个被合并的 `fun_xxx` 和 `var_xxx` 模块添加 `use` 别名声明，以确保原有代码中对这些模块的引用仍然有效。

## 背景说明
- `merge` 前：目录结构为 `mod_xxx/fun_foo.rs`、`mod_xxx/var_bar.rs`
- `merge` 后：所有内容合并到 `mod_xxx.rs`，`fun_foo` 和 `var_bar` 模块不再存在
- 需要在 `lib.rs` 中生成 `use mod_xxx as fun_foo;` 和 `use mod_xxx as var_bar;` 别名
- **不生成** `pub use mod_xxx::*;`，避免将模块所有内容导出

## 涉及的文件和模块
- **主要修改文件**：`src/merge.rs`
- **关键函数**：
  - `Feature::merge` (第 69-82 行)
  - `Feature::merge_file` (第 84-143 行)
  - `Feature::deduplicate_mod_rs` (第 425-456 行)
  - `Feature::generate_lib_rs` (第 578-631 行)
- **生成的文件**：`rust/src.2/lib.rs`

## 技术选型或修改思路

### 优化方案
- **不重新解析** `mod_xxx.rs` 文件
- **在 `merge_file` 中直接收集**：从文件名（`fun_*.rs` 和 `var_*.rs`）提取别名信息
- **传递给 `generate_lib_rs`**：通过返回值和参数链传递别名映射
- **移除 `pub use mod_xxx::*;`**：只保留 `mod` 声明和别名声明

### 实现步骤

#### 1. 修改 `merge_file` 函数签名和实现
- 修改返回值：`Result<bool>` → `Result<(bool, Vec<String>)>`
- 返回元组中第二个元素为该模块的别名列表（如 `["fun_foo", "var_bar"]`）
- 实现方式：在 `collect_rust_files` 后，从 `rs_files` 中提取文件名，去掉扩展名 `.rs`

```rust
fn merge_file(&self, file: &File) -> Result<(bool, Vec<String>)> {
    let mod_name = Self::get_mod_name_for_file(&self.prefix, file)?;
    let mod_dir = self.root.join("rust/src").join(&mod_name);

    if !mod_dir.exists() {
        return Ok((false, vec![]));
    }

    let rs_files = Self::collect_rust_files(&mod_dir)?;

    // 收集别名
    let aliases: Vec<String> = rs_files
        .iter()
        .filter_map(|path| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .collect();

    // ... 合并逻辑 ...

    Ok((true, aliases))
}
```

#### 2. 修改 `merge` 函数
- 收集所有模块的别名映射：`HashMap<String, Vec<String>>`
- 键为模块名（如 `mod_xxx`），值为别名列表
- 将此映射传递给 `deduplicate_mod_rs`

```rust
pub fn merge(&mut self) -> Result<()> {
    let mut all_aliases: HashMap<String, Vec<String>> = HashMap::new();

    for file in &self.files {
        let mod_name = Self::get_mod_name_for_file(&self.prefix, file)?;
        let (_, aliases) = self.merge_file(file)?;
        all_aliases.insert(mod_name, aliases);
    }

    self.deduplicate_mod_rs(&all_aliases)?;
    self.link_src()?;
    Ok(())
}
```

#### 3. 修改 `deduplicate_mod_rs` 函数
- 新增参数：`aliases: &HashMap<String, Vec<String>>`
- 将 `aliases` 传递给 `generate_lib_rs`

```rust
fn deduplicate_mod_rs(&self, aliases: &HashMap<String, Vec<String>>) -> Result<()> {
    // ...
    Self::generate_lib_rs(&src_2, &mod_files, &duplicates, &collected.foreign_mod_template, aliases)?;
    // ...
}
```

#### 4. 修改 `generate_lib_rs` 函数
- 新增参数：`aliases: &HashMap<String, Vec<String>>`
- 在生成 `mod` 声明之前，生成对应的 `use` 别名声明
- **移除** `pub use mod_name::*;` 语句
- 只保留 `mod mod_name;` 和别名声明

```rust
fn generate_lib_rs(
    src_2: &Path,
    mod_files: &[PathBuf],
    duplicates: &Duplicates,
    foreign_mod_template: &Option<syn::ItemForeignMod>,
    aliases: &HashMap<String, Vec<String>>,
) -> Result<()> {
    // ...

    for mod_file in mod_files {
        let mod_name = mod_file.file_stem().unwrap().to_string_lossy().to_string();

        // 生成别名声明
        if let Some(mod_aliases) = aliases.get(&mod_name) {
            for alias in mod_aliases {
                lib_file.items.push(
                    syn::parse_str(&format!("use {mod_name} as {alias};")).unwrap()
                );
            }
        }

        lib_file.items.push(syn::parse_str(&format!("mod {mod_name};")).unwrap());
        // 移除：lib_file.items.push(syn::parse_str(&format!("pub use {mod_name}::*;")).unwrap());
    }

    // ...
}
```

### 别名生成规则
- 从文件名直接提取：`fun_foo.rs` → 别名 `fun_foo`
- 从文件名直接提取：`var_bar.rs` → 别名 `var_bar`
- 无需解析文件内容，仅使用文件名

## 预期的测试用例

### 1. 基础功能测试
- 模拟 `mod_test` 目录包含 `fun_foo.rs` 和 `var_bar.rs`
- 验证生成的 `lib.rs` 包含：
  ```rust
  use mod_test as fun_foo;
  use mod_test as var_bar;
  mod mod_test;
  ```
- 验证**不包含** `pub use mod_test::*;`

### 2. 边界情况测试
- 模块中只有函数（无变量）：只生成 `fun_*` 别名
- 模块中只有变量（无函数）：只生成 `var_*` 别名
- 空模块：不生成别名
- 多个模块：每个模块生成各自的别名，按模块顺序排列

### 3. 集成测试
- 运行完整的 `Feature::merge` 流程
- 验证生成的 `lib.rs` 编译通过
- 验证原有对 `fun_xxx` 和 `var_xxx` 的引用仍然有效