# 实施方案：将别名声明从 lib.rs 移到各模块内部

## 目标概述
解决多个 mod_xxx 下有重复子模块导致的别名冲突问题。将别名声明从 lib.rs 移到各个 mod_xxx.rs 文件中，避免别名在 lib.rs 作用域内冲突。

## 背景说明
- **当前问题**：
  - 多个 mod_xxx 下可能有重复的子模块（如 `mod_a::fun_foo`, `mod_b::fun_foo`）
  - 当前在 lib.rs 中生成别名声明：`use mod_a as fun_foo; use mod_b as fun_foo;`
  - 这会导致别名冲突，因为同一个别名 `fun_foo` 被使用两次
- **新方案**：
  - 不在 lib.rs 中生成别名声明
  - 在每个 mod_xxx.rs 文件中生成别名声明
  - 例如：在 mod_a.rs 中 `use super::mod_a as fun_foo;`

## 涉及的文件和模块
- **主要修改文件**：`src/merge.rs`
- **关键函数**：
  - `Feature::merge` (第 69-86 行)
  - `Feature::merge_file` (第 88-149 行)
  - `Feature::generate_lib_rs` (约第 588-648 行)

## 技术选型或修改思路

### 实现步骤

#### 1. 修改 `merge_file` 函数
- 返回值：`Result<(bool, Vec<String>)>` → `Result<bool>`
- 在构建合并后的文件时，添加别名声明
- 在 `use super::*;` 之后添加 `use super::{mod_name} as {alias};`

#### 2. 修改 `merge` 函数
- 移除收集别名的逻辑（不再需要）
- 调用 `merge_file` 时不再需要别名列表

#### 3. 修改 `deduplicate_mod_rs` 和 `generate_lib_rs` 函数
- 移除 `aliases` 参数
- 不再生成别名声明

### 代码实现方案

```rust
// 修改 merge_file 函数
fn merge_file(&self, file: &File) -> Result<bool> {
    let mod_name = Self::get_mod_name_for_file(&self.prefix, file)?;
    let mod_dir = self.root.join("rust/src").join(&mod_name);

    if !mod_dir.exists() {
        return Ok(false);
    }

    println!("Processing mod for merge: {}", mod_name);

    let module_names = Self::collect_modules_from_mod_rs(&mod_dir)?;

    println!(
        "Merging {} modules for mod: {} ...",
        module_names.len(),
        mod_name
    );

    if module_names.is_empty() {
        return Err(Error::inval());
    }

    let mut items = Vec::new();
    let mut deps = DepNames::new();

    for module_name in &module_names {
        let rs_file = mod_dir.join(module_name).with_extension("rs");
        Self::parse_rust_file(&rs_file, &mut items, &mut deps)?;
    }

    let mod_rs = mod_dir.join("mod.rs");
    let (type_items, foreign_mod) = Self::extract_dependencies(&mod_rs, &mut deps)?;

    let mut merged_items = Vec::new();
    merged_items.push(syn::parse2(quote! { use super::*; }).unwrap());
    
    // 添加别名声明
    for alias in &module_names {
        merged_items.push(
            syn::parse_str(&format!("use super::{mod_name} as {alias};")).unwrap()
        );
    }
    
    merged_items.extend(type_items);
    if let Some(fm) = foreign_mod {
        merged_items.push(syn::Item::ForeignMod(fm));
    }
    merged_items.extend(items);

    let merged_file = syn::File {
        shebang: None,
        attrs: Vec::new(),
        items: merged_items,
    };

    let formatted = prettyplease::unparse(&merged_file);

    let merged_mod_dir = self.root.join("rust/src.2");
    let _ = fs::create_dir(&merged_mod_dir);
    let merged_rs = merged_mod_dir.join(&mod_name).with_extension("rs");
    fs::write(&merged_rs, formatted.as_bytes())
        .log_err(&format!("write {}", merged_rs.display()))?;

    println!("File merged successfully: {}", merged_rs.display());

    Ok(true)
}

// 修改 merge 函数
pub fn merge(&mut self) -> Result<()> {
    println!("Starting merge for feature '{}'", self.name);

    for file in &self.files {
        self.merge_file(file)?;
    }

    self.deduplicate_mod_rs()?;

    self.link_src()?;

    println!("Feature '{}' merged successfully", self.name);
    Ok(())
}

// 修改 deduplicate_mod_rs 函数签名
fn deduplicate_mod_rs(&self) -> Result<()> {
    let src_2 = self.root.join("rust/src.2");
    if !src_2.exists() {
        return Ok(());
    }

    let mod_files = Self::collect_mod_files(&src_2)?;
    if mod_files.is_empty() {
        return Ok(());
    }

    let collected = Self::collect_items_from_files(&mod_files)?;
    let duplicates = Self::find_duplicates(&collected.named_items, &collected.ffi_items);

    Self::generate_lib_rs(&src_2, &mod_files, &duplicates, &collected.foreign_mod_template)?;

    if !duplicates.named_remove_set.is_empty() || !duplicates.ffi_remove_set.is_empty() {
        Self::remove_duplicates_from_files(&mod_files, &duplicates)?;
    }

    println!(
        "Deduplicated {} types and {} FFI declarations to lib.rs",
        duplicates.named_remove_set.len(),
        duplicates.ffi_remove_set.len()
    );
    Ok(())
}

// 修改 generate_lib_rs 函数签名
fn generate_lib_rs(
    src_2: &Path,
    mod_files: &[PathBuf],
    duplicates: &Duplicates,
    foreign_mod_template: &Option<syn::ItemForeignMod>,
) -> Result<()> {
    let mut lib_items: Vec<syn::Item> = Vec::new();
    lib_items.extend(duplicates.named_to_extract.clone());

    if !duplicates.ffi_to_extract.is_empty() {
        if let Some(mut fm) = foreign_mod_template.clone() {
            fm.items = duplicates.ffi_to_extract.clone();
            lib_items.push(syn::Item::ForeignMod(fm));
        }
    }

    let attr_str = r#"#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(improper_ctypes)]
#![allow(dead_code)]
#![allow(unused_imports)]"#;
    let attrs: Vec<syn::Attribute> = syn::Attribute::parse_inner
        .parse_str(attr_str)
        .unwrap_or_default();

    let mut lib_file = syn::File {
        shebang: None,
        attrs,
        items: vec![syn::parse_str("use core::ffi::*;").unwrap()],
    };

    lib_file.items.extend(lib_items);

    // 只生成 mod 声明，不生成别名
    for mod_file in mod_files {
        let mod_name = mod_file.file_stem().unwrap().to_string_lossy().to_string();
        lib_file
            .items
            .push(syn::parse_str(&format!("mod {mod_name};")).unwrap());
    }

    let lib_content = prettyplease::unparse(&lib_file);
    let lib_rs_path = src_2.join("lib.rs");
    fs::write(&lib_rs_path, lib_content.as_bytes())
        .log_err(&format!("write {}", lib_rs_path.display()))?;

    Ok(())
}
```

## 优势
1. **解决别名冲突**：每个模块的别名在自己的作用域内，不会与其他模块冲突
2. **更符合 Rust 模块系统**：别名在模块内部定义，使用 `super::` 引用父模块
3. **代码简化**：不需要在 lib.rs 中管理别名映射

## 预期的测试用例

### 1. 基础功能测试
- 创建 mod_a 和 mod_b，两者都有 fun_foo
- 验证生成的 mod_a.rs 包含 `use super::mod_a as fun_foo;`
- 验证生成的 mod_b.rs 包含 `use super::mod_b as fun_foo;`
- 验证 lib.rs 不包含别名声明

### 2. 边界情况测试
- 模块中无子模块：不生成别名声明
- 模块中有多个子模块：生成多个别名声明
- 模块中只有 fun_ 模块：只生成 fun_ 别名
- 模块中只有 var_ 模块：只生成 var_ 别名

### 3. 集成测试
- 运行完整的 `Feature::merge` 流程
- 验证生成的代码可以编译通过
- 验证原有对 `fun_xxx` 和 `var_xxx` 的引用仍然有效