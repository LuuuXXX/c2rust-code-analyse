# 实施方案：从 mod.rs 解析模块声明替代文件系统扫描

## 目标概述
修改 `Feature::merge` 流程，不再从文件系统目录扫描 `fun_*.rs` 和 `var_*.rs` 文件，而是通过解析 `mod.rs` 文件中的 `mod fun_xxx;` 和 `mod var_xxx;` 语句来提取需要合并的模块名称列表。

## 背景说明
- **当前实现**：使用 `WalkDir` 扫描 `mod_xxx/` 目录，收集所有 `fun_*.rs` 和 `var_*.rs` 文件路径
- **问题**：依赖文件系统结构，不够精确，可能与 `mod.rs` 中的模块声明不一致
- **新方案**：解析 `mod.rs` 文件，提取 `mod` 声明的模块名称，不返回完整路径

## 涉及的文件和模块
- **主要修改文件**：`src/merge.rs`
- **关键函数**：
  - `Feature::merge_file` (第 88-152 行)
  - `Feature::collect_rust_files` (第 154-164 行) - **需要重构**

## 技术选型或修改思路

### 实现步骤

#### 1. 重构 `collect_rust_files` 函数
- 修改函数签名：`fn collect_rust_files(mod_dir: &Path) -> Result<Vec<PathBuf>>`
- 改为：`fn collect_modules_from_mod_rs(mod_dir: &Path) -> Result<Vec<String>>`
- 返回值：模块名称列表（如 `["fun_foo", "var_bar"]`）
- 实现方式：
  - 读取 `mod_dir/mod.rs` 文件
  - 使用 `syn::parse_file` 解析为 AST
  - 遍历 `items`，提取 `syn::Item::Mod` 项
  - 过滤出标识符以 `fun_` 或 `var_` 开头的模块
  - 返回模块名称列表

#### 2. 修改 `merge_file` 函数
- 调用新的 `collect_modules_from_mod_rs` 函数获取模块名称列表
- 在循环中，根据模块名和 mod_dir 构造 PathBuf
- 直接使用模块名列表作为别名列表

### 代码实现方案

```rust
// 新函数：从 mod.rs 解析模块声明，返回模块名称列表
fn collect_modules_from_mod_rs(mod_dir: &Path) -> Result<Vec<String>> {
    let mod_rs_path = mod_dir.join("mod.rs");
    
    if !mod_rs_path.exists() {
        return Ok(vec![]);
    }
    
    let content = fs::read_to_string(&mod_rs_path)
        .log_err(&format!("read {}", mod_rs_path.display()))?;
    
    let ast = syn::parse_file(&content)
        .log_err(&format!("parse {}", mod_rs_path.display()))?;
    
    let mut modules = Vec::new();
    
    for item in ast.items {
        if let syn::Item::Mod(mod_item) = item {
            let mod_name = mod_item.ident.to_string();
            
            // 只处理 fun_ 和 var_ 开头的模块
            if mod_name.starts_with("fun_") || mod_name.starts_with("var_") {
                // 检查对应的 rs 文件是否存在
                let rs_file = mod_dir.join(&mod_name).with_extension("rs");
                if rs_file.exists() {
                    modules.push(mod_name);
                }
            }
        }
    }
    
    Ok(modules)
}

// 修改 merge_file 函数
fn merge_file(&self, file: &File) -> Result<(bool, Vec<String>)> {
    let mod_name = Self::get_mod_name_for_file(&self.prefix, file)?;
    let mod_dir = self.root.join("rust/src").join(&mod_name);

    if !mod_dir.exists() {
        return Ok((false, vec![]));
    }

    println!("Processing mod for merge: {}", mod_name);
    
    // 从 mod.rs 解析模块声明，获取模块名称列表
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

    // 根据模块名构造文件路径并解析
    for module_name in &module_names {
        let rs_file = mod_dir.join(module_name).with_extension("rs");
        Self::parse_rust_file(&rs_file, &mut items, &mut deps)?;
    }

    // 从 mod.rs 提取该模块依赖的类型和 FFI
    let mod_rs = mod_dir.join("mod.rs");
    let (type_items, foreign_mod) = Self::extract_dependencies(&mod_rs, &mut deps)?;

    // 构建合并后的文件
    let mut merged_items = Vec::new();
    merged_items.push(syn::parse2(quote! { use super::*; }).unwrap());
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

    Ok((true, module_names))
}

// 删除旧的 collect_rust_files 函数
```

## 优势
1. **更准确**：直接从 `mod.rs` 的模块声明确定要合并的文件，与 Rust 模块系统完全一致
2. **更可靠**：不依赖文件系统扫描，避免因文件系统状态不一致导致的问题
3. **职责分离**：`collect_modules_from_mod_rs` 只负责收集模块名，路径构造由调用方负责
4. **代码简化**：模块名列表直接作为别名列表使用，无需额外处理

## 预期的测试用例

### 1. 基础功能测试
- 创建模拟的 `mod.rs`，包含 `mod fun_foo;` 和 `mod var_bar;`
- 创建对应的 `fun_foo.rs` 和 `var_bar.rs` 文件
- 验证 `collect_modules_from_mod_rs` 正确返回 `["fun_foo", "var_bar"]`
- 验证合并后的 `mod_xxx.rs` 包含正确内容

### 2. 边界情况测试
- `mod.rs` 中只有 `fun_` 模块（无 `var_`）：只返回函数模块名
- `mod.rs` 中只有 `var_` 模块（无 `fun_`）：只返回变量模块名
- `mod.rs` 中没有 `mod` 声明：返回空列表
- `mod.rs` 中有其他类型的 `mod` 声明（不以 `fun_` 或 `var_` 开头）：忽略

### 3. 错误处理测试
- `mod.rs` 文件不存在：返回空列表
- `mod.rs` 声明的模块文件不存在：跳过该模块
- `mod.rs` 语法错误：返回解析错误

### 4. 集成测试
- 运行完整的 `Feature::merge` 流程
- 验证生成的 `lib.rs` 包含正确的别名声明
- 验证所有模块正确合并