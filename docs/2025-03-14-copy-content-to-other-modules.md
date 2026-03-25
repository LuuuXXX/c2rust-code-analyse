# 实施方案：Feature::update 时自动拷贝内容到同名空文件

## 目标概述
在 `Feature::update` 时，如果当前文件的 Rust 代码不为空，且函数名/变量名不同于 export_name（以 `_c2rust_private_` 开头），则查找其他模块下的同名文件。如果存在同名文件且内容为空，并且两个模块下同名 C 文件的内容完全相同，则将当前文件的内容拷贝过去。

### 涉及的文件和模块
- **主要文件**：`src/feature.rs`
- **关键方法**：
  - `Feature::update`（第 170-253 行）
  - `Feature::validate_file`（第 725-763 行）
  - 新增辅助方法：`copy_content_to_other_modules`

## 技术选型与修改思路

### 需求分析

根据代码分析，涉及的命名规则：
- **export_name**：原始 C 符号名（如 `"foo"`），存储在 `#[unsafe(export_name = "foo")]` 属性中
- **link_name**：经过重命名后的符号名，可能以 `_c2rust_private_` 开头
- **函数名/变量名**：Rust 代码中使用的名称（如 `"foo"`）
- **文件名**：带前缀的文件名（如 `"fun_foo.rs"`, `"var_bar.rs"`）
- **C 文件名**：同名的 `.c` 文件（如 `"fun_foo.c"`, `"var_bar.c"`）

### 触发条件

满足以下条件时触发拷贝：
1. 当前 Rust 文件内容不为空（`validate_file` 返回 `true`）
2. `link_name` 以 `_c2rust_private_` 开头（即经过重命名）
3. 找到其他模块下的同名文件
4. 同名文件内容为空（文件不存在或内容为空）
5. 当前模块和目标模块的同名 C 文件内容完全相同

## 具体实现步骤

### 步骤 1：新增辅助方法 `copy_content_to_other_modules`

```rust
fn copy_content_to_other_modules(file: &Path) -> Result<()> {
    // 1. 读取当前 Rust 文件内容
    let rust_content = fs::read_to_string(file)
        .log_err(&format!("read {}", file.display()))?;
    
    // 2. 获取当前 C 文件路径
    let source_c_file = file.with_extension("c");
    if !source_c_file.exists() {
        return Ok(());
    }
    
    // 3. 读取当前 C 文件内容
    let c_content = fs::read_to_string(&source_c_file)
        .log_err(&format!("read {}", source_c_file.display()))?;
    
    // 4. 获取当前模块目录（跳过）
    let current_mod_dir = file.parent().ok_or(Error::inval())?;
    
    // 5. 获取 src 目录
    let src_dir = current_mod_dir.parent().ok_or(Error::inval())?;
    
    // 6. 获取文件名
    let file_name = file
        .file_name()
        .ok_or(Error::inval())?
        .to_string_lossy()
        .to_string();
    
    // 7. 使用 WalkDir 遍历 src 目录下一层目录
    for entry in WalkDir::new(&src_dir)
        .min_depth(1)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let mod_dir = entry.path();
        
        // 跳过非目录或非 mod_ 开头的目录
        if !mod_dir.is_dir() {
            continue;
        }
        let mod_name = mod_dir
            .file_name()
            .ok_or(Error::inval())?
            .to_string_lossy()
            .to_string();
        if !mod_name.starts_with("mod_") {
            continue;
        }
        
        // 跳过当前模块
        if mod_dir == current_mod_dir {
            continue;
        }
        
        // 查找同名文件
        let target_rust_file = mod_dir.join(&file_name);
        if !target_rust_file.exists() {
            continue;
        }
        
        // 检查目标文件内容是否为空
        let target_rust_content = fs::read_to_string(&target_rust_file)
            .log_err(&format!("read {}", target_rust_file.display()))?;
        if !target_rust_content.trim().is_empty() {
            continue;
        }
        
        // 检查 C 文件是否存在且内容相同
        let target_c_file = target_rust_file.with_extension("c");
        if !target_c_file.exists() {
            continue;
        }
        let target_c_content = fs::read_to_string(&target_c_file)
            .log_err(&format!("read {}", target_c_file.display()))?;
        if c_content != target_c_content {
            continue;
        }
        
        // 拷贝内容
        fs::write(&target_rust_file, rust_content.as_bytes())
            .log_err(&format!("write {}", target_rust_file.display()))?;
        
        println!(
            "Copied content from {} to {}",
            file.display(),
            target_rust_file.display()
        );
    }
    
    Ok(())
}
```

### 步骤 2：修改 `validate_file` 方法

在 `validate_file` 方法的最后（返回 `Ok(true)` 之前），添加拷贝逻辑：

```rust
fn validate_file(file: &Path, link_name: &str, prefix: &str) -> Result<bool> {
    // ... 现有逻辑 ...
    
    if is_changed {
        let formatted = prettyplease::unparse(&ast);
        fs::write(file, formatted.as_bytes()).log_err(&format!("write {}", file.display()))?;
    }
    
    // 新增：检查是否需要拷贝到其他模块
    // 只有当 link_name 以 _c2rust_private_ 开头时才触发拷贝
    if link_name.starts_with("_c2rust_private_") {
        let _ = Self::copy_content_to_other_modules(file);
    }
    
    Ok(true)
}
```

## 预期的测试用例

### 测试用例 1：基本拷贝场景
```rust
#[test]
fn test_copy_content_to_other_modules_basic() {
    // 场景：mod_a/fun_foo.rs 内容不为空，mod_b/fun_foo.rs 内容为空
    // 且 link_name 以 _c2rust_private_ 开头
    // 且两个模块下的 fun_foo.c 内容相同
    // 预期：将 mod_a/fun_foo.rs 内容拷贝到 mod_b/fun_foo.rs
    
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    
    // 创建目录结构
    let mod_a = root.join("rust/src/mod_a");
    let mod_b = root.join("rust/src/mod_b");
    fs::create_dir_all(&mod_a).unwrap();
    fs::create_dir_all(&mod_b).unwrap();
    
    // 创建文件
    let fun_foo_a_rs = mod_a.join("fun_foo.rs");
    let fun_foo_b_rs = mod_b.join("fun_foo.rs");
    let fun_foo_a_c = mod_a.join("fun_foo.c");
    let fun_foo_b_c = mod_b.join("fun_foo.c");
    
    // 写入内容
    fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
    fs::write(&fun_foo_b_rs, "").unwrap();
    fs::write(&fun_foo_a_c, "int foo() { return 42; }").unwrap();
    fs::write(&fun_foo_b_c, "int foo() { return 42; }").unwrap();
    
    // 执行拷贝
    Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();
    
    // 验证
    let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
    assert_eq!(content_b.trim(), "pub fn foo() -> i32 { 42 }");
}
```

### 测试用例 2：C 文件内容不同，不拷贝
```rust
#[test]
fn test_copy_content_c_content_different() {
    // 场景：C 文件内容不同，不应拷贝
    
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    
    let mod_a = root.join("rust/src/mod_a");
    let mod_b = root.join("rust/src/mod_b");
    fs::create_dir_all(&mod_a).unwrap();
    fs::create_dir_all(&mod_b).unwrap();
    
    let fun_foo_a_rs = mod_a.join("fun_foo.rs");
    let fun_foo_b_rs = mod_b.join("fun_foo.rs");
    let fun_foo_a_c = mod_a.join("fun_foo.c");
    let fun_foo_b_c = mod_b.join("fun_foo.c");
    
    fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
    fs::write(&fun_foo_b_rs, "").unwrap();
    fs::write(&fun_foo_a_c, "int foo() { return 42; }").unwrap();
    fs::write(&fun_foo_b_c, "int foo() { return 100; }").unwrap(); // 内容不同
    
    Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();
    
    // 验证未被拷贝
    let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
    assert!(content_b.trim().is_empty());
}
```

### 测试用例 3：目标文件不为空，不拷贝
```rust
#[test]
fn test_copy_content_target_not_empty() {
    // 场景：目标文件已有内容，不应拷贝
    
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    
    let mod_a = root.join("rust/src/mod_a");
    let mod_b = root.join("rust/src/mod_b");
    fs::create_dir_all(&mod_a).unwrap();
    fs::create_dir_all(&mod_b).unwrap();
    
    let fun_foo_a_rs = mod_a.join("fun_foo.rs");
    let fun_foo_b_rs = mod_b.join("fun_foo.rs");
    let fun_foo_a_c = mod_a.join("fun_foo.c");
    let fun_foo_b_c = mod_b.join("fun_foo.c");
    
    fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
    fs::write(&fun_foo_b_rs, "pub fn bar() -> i32 { 100 }").unwrap(); // 已有内容
    fs::write(&fun_foo_a_c, "int foo() { return 42; }").unwrap();
    fs::write(&fun_foo_b_c, "int foo() { return 42; }").unwrap();
    
    Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();
    
    // 验证未被覆盖
    let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
    assert_eq!(content_b.trim(), "pub fn bar() -> i32 { 100 }");
}
```

### 测试用例 4：源 C 文件不存在，不拷贝
```rust
#[test]
fn test_copy_content_source_c_not_exist() {
    // 场景：源 C 文件不存在，不应拷贝
    
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    
    let mod_a = root.join("rust/src/mod_a");
    let mod_b = root.join("rust/src/mod_b");
    fs::create_dir_all(&mod_a).unwrap();
    fs::create_dir_all(&mod_b).unwrap();
    
    let fun_foo_a_rs = mod_a.join("fun_foo.rs");
    let fun_foo_b_rs = mod_b.join("fun_foo.rs");
    // 不创建 fun_foo_a_c
    
    fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
    fs::write(&fun_foo_b_rs, "").unwrap();
    
    Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();
    
    // 验证未被拷贝
    let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
    assert!(content_b.trim().is_empty());
}
```

## 实现细节总结

1. **新增方法**：
   - `copy_content_to_other_modules(file: &Path)`：只接收一个参数，内部处理所有逻辑

2. **修改方法**：
   - `validate_file`：在返回前判断 `link_name.starts_with("_c2rust_private_")`，如果是才调用拷贝函数

3. **关键逻辑**：
   - 在 `validate_file` 中判断 `link_name` 是否以 `_c2rust_private_` 开头
   - 只有以该前缀开头的才调用 `copy_content_to_other_modules`
   - `copy_content_to_other_modules` 只接收一个参数 `file`
   - 使用 `file.parent().parent()` 获取 src 目录
   - 使用 WalkDir 遍历 src 目录下一层目录
   - 内部处理 C 文件逻辑
   - 比较同名文件和同名 C 文件
   - 只拷贝内容到空文件
   - 确保 C 文件内容相同

4. **简化点**：
   - 前缀判断放在 `validate_file` 中
   - 拷贝函数专注于拷贝逻辑
   - 无需 `get_src_dir` 方法，直接使用 `file.parent().parent()`
   - 无需解析 AST 提取 export_name
   - 代码最简洁