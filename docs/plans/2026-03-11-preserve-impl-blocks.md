# 实施方案：保留 mod.rs 中的 impl 语句块

## 目标概述
在 `Feature::merge` 时，保留 `mod.rs` 中与最终保留下来的类型（`dep_types`）对应的 `impl` 语句块，并确保 impl 块紧跟在对应的类型定义之后。

## 背景说明
- `impl` 语句块在 `mod.rs` 中定义，为特定类型提供方法实现
- 只保留最终保留下来的类型（`dep_types`）对应的 impl 块
- impl 块需要与类型定义放在一起，而不是单独列在最后

## 涉及的文件和模块
- **主要修改文件**：`src/merge.rs`
- **关键函数**：
  - `Feature::extract_dependencies` (第 326-385 行)
  - `Feature::merge_file` (第 84-147 行)

## 技术选型或修改思路

### 实现步骤

#### 1. 添加新函数：获取 impl 的 self_type 名称
- 函数名：`impl_self_type_name`
- 输入：`&syn::ItemImpl`
- 输出：`Option<String>`（类型名称）
- 实现方式：从 `impl_item.self_ty` 中提取路径的最后一个标识符

#### 2. 修改 `extract_dependencies` 函数
- 收集所有 `syn::Item::Impl` 项
- 返回值改为：`(Vec<syn::Item>, Vec<syn::ItemImpl>, Option<syn::ItemForeignMod>)`
- 返回所有 impl 块，未过滤

#### 3. 修改 `merge_file` 函数
- 调用 `extract_dependencies` 获取所有 impl 块
- 根据 impl 的 self_type 将 impl 块分组到 `HashMap<String, Vec<syn::ItemImpl>>`
- 在遍历 `type_items` 时，查找是否有对应的 impl 块
- 如果有，在类型定义后立即添加 impl 块

### 代码实现方案

```rust
// 添加新函数：获取 impl 的 self_type 名称
fn impl_self_type_name(impl_item: &syn::ItemImpl) -> Option<String> {
    match &impl_item.self_ty {
        syn::Type::Path(type_path) if type_path.qself.is_none() => {
            if let Some(segment) = type_path.path.segments.last() {
                Some(segment.ident.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

// 修改 extract_dependencies 函数
fn extract_dependencies(
    mod_rs: &Path,
    deps: &mut DepNames,
) -> Result<(Vec<syn::Item>, Vec<syn::ItemImpl>, Option<syn::ItemForeignMod>)> {
    let content = fs::read_to_string(mod_rs).log_err(&format!("read {}", mod_rs.display()))?;
    let ast = syn::parse_file(&content).log_err(&format!("parse {}", mod_rs.display()))?;

    let mut all_types: HashMap<String, syn::Item> = HashMap::new();
    let mut all_ffi: HashMap<String, syn::ForeignItem> = HashMap::new();
    let mut foreign_mod_template: Option<syn::ItemForeignMod> = None;
    let mut all_impls: Vec<syn::ItemImpl> = Vec::new();

    for item in ast.items {
        match item {
            syn::Item::ForeignMod(ref fm) => {
                if foreign_mod_template.is_none() {
                    let mut template = fm.clone();
                    template.items.clear();
                    foreign_mod_template = Some(template);
                }
                for ffi_item in fm.items.clone() {
                    if let Some(name) = Self::foreign_item_name(&ffi_item) {
                        all_ffi.insert(name, ffi_item);
                    }
                }
            }
            syn::Item::Impl(impl_item) => {
                all_impls.push(impl_item);
            }
            _ => {
                if let Some(name) = Self::item_name(&item) {
                    all_types.insert(name, item);
                }
            }
        }
    }

    all_types.insert(
        "c_size_t".to_string(),
        syn::parse_str("type c_size_t = usize;").unwrap(),
    );
    all_types.insert(
        "c_ssize_t".to_string(),
        syn::parse_str("type c_ssize_t = isize;").unwrap(),
    );
    all_types.insert(
        "c_ptrdiff_t".to_string(),
        syn::parse_str("type c_ptrdiff_t = isize;").unwrap(),
    );

    let mut dep_types = Vec::new();
    let mut dep_ffi = Vec::new();
    Self::filter_dependencies(all_types, all_ffi, deps, &mut dep_types, &mut dep_ffi);

    let foreign_mod = if !dep_ffi.is_empty() {
        let mut fm = foreign_mod_template.unwrap();
        fm.items = dep_ffi;
        Some(fm)
    } else {
        None
    };

    Ok((dep_types, all_impls, foreign_mod))
}

// 修改 merge_file 函数
fn merge_file(&self, file: &File) -> Result<bool> {
    // ... 前面的代码不变 ...

    let mod_rs = mod_dir.join("mod.rs");
    let (type_items, all_impls, foreign_mod) = Self::extract_dependencies(&mod_rs, &mut deps)?;

    // 收集 dep_types 中的类型名称
    let dep_type_names: HashSet<String> = type_items
        .iter()
        .filter_map(|item| Self::item_name(item))
        .collect();

    // 过滤 impl 块，只保留 self_type 在 dep_types 中的 impl
    let filtered_impls: Vec<syn::ItemImpl> = all_impls
        .into_iter()
        .filter(|impl_item| {
            Self::impl_self_type_name(impl_item)
                .map(|name| dep_type_names.contains(&name))
                .unwrap_or(false)
        })
        .collect();

    // 根据 impl 的 self_type 将 impl 块分组
    let mut impl_map: HashMap<String, Vec<syn::ItemImpl>> = HashMap::new();
    for impl_block in filtered_impls {
        if let Some(type_name) = Self::impl_self_type_name(&impl_block) {
            impl_map.entry(type_name).or_default().push(impl_block);
        }
    }

    let mut merged_items = Vec::new();
    merged_items.push(syn::parse2(quote! { use super::*; }).unwrap());

    for alias in &module_names {
        merged_items.push(
            syn::parse_str(&format!("use super::{mod_name} as {alias};")).unwrap(),
        );
    }

    // 添加类型定义和对应的 impl 块
    for type_item in &type_items {
        merged_items.push(type_item.clone());
        
        if let Some(type_name) = Self::item_name(type_item) {
            if let Some(impl_blocks) = impl_map.get(&type_name) {
                for impl_block in impl_blocks {
                    merged_items.push(syn::Item::Impl(impl_block.clone()));
                }
            }
        }
    }
    
    if let Some(fm) = foreign_mod {
        merged_items.push(syn::Item::ForeignMod(fm));
    }
    merged_items.extend(items);

    // ... 后面的代码不变 ...
}
```

## 预期的测试用例

### 1. 基础功能测试
- 创建 mod.rs，包含类型定义和 impl 块
- 验证 impl 块被保留并紧跟在类型定义之后

### 2. 过滤测试
- 创建 mod.rs，包含多个类型和对应的 impl
- 只保留部分类型（通过 deps 依赖关系）
- 验证只有被保留的类型对应的 impl 块被保留

### 3. 无 impl 测试
- 创建 mod.rs，只有类型定义，没有 impl 块
- 验证代码正常生成

### 4. 复杂类型测试
- 测试 impl 块的 self_type 是复杂路径的情况
- 验证能正确提取类型名称