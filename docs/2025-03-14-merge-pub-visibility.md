# 实施方案：merge 后只有 pub 函数/静态变量签名依赖的类型（包括间接依赖）才能是 pub

## 目标概述
修改 `Feature::merge` 方法，在合并后的 `mod_xxx.rs` 文件中，只有被 **pub 函数/静态变量签名**依赖的类型（包括间接依赖）才能保持 `pub`，其他类型应设置为 `pub(crate)`。

**关键修正**：
- `PubDepVisitor` 使用 `insert(name, true)` 直接覆盖已存在的 `false`
- 将仅在函数体中使用的类型提升为 pub（如果它是 pub 签名的间接依赖）

## 涉及的文件和模块
- **主要文件**：`src/merge.rs`
- **关键方法**：
  - `DepNames` 结构（第 12-15 行）
  - `Feature::parse_rust_file`（第 214-270 行）
  - `Feature::filter_dependencies`（第 440-470 行）
  - `Feature::merge_file`（第 149-159 行）

## 技术选型与修改思路

### 核心逻辑
1. **`used_names: HashMap<String, bool>`**：
   - `true` = 类型是 pub 签名依赖（直接或间接）
   - `false` = 类型仅被函数体或 private 函数使用

2. **`PubDepVisitor` 直接覆盖**：
   - 使用 `insert(name, true)` 覆盖已存在的 `false`
   - 将仅在函数体中使用的类型提升为 pub（如果它是 pub 签名的间接依赖）

3. **递归传播 pub 标记**：
   - 在 `filter_dependencies` 中，收集类型时，如果是 pub，使用 `PubDepVisitor` 收集其内部依赖

## 具体实现步骤

### 步骤 1：修改 `DepNames` 结构
```rust
struct DepNames {
    used_names: HashMap<String, bool>,  // bool 表示是否是 pub 签名依赖
    mac_tokens: String,
}

impl DepNames {
    fn new() -> Self {
        Self {
            used_names: HashMap::new(),
            mac_tokens: String::new(),
        }
    }

    fn contains(&self, name: &str) -> bool {
        if self.used_names.contains_key(name) {
            return true;
        }
        let regex = regex::Regex::new(&format!("[^a-zA-Z_]{name}[^a-zA-Z_]")).unwrap();
        regex.find(&self.mac_tokens).is_some()
    }

    fn mark_used(&mut self, name: String) {
        self.used_names.entry(name).or_insert(false);
    }

    fn mark_pub(&mut self, name: String) {
        self.used_names.insert(name, true);
    }

    fn is_pub(&self, name: &str) -> bool {
        self.used_names.get(name).copied().unwrap_or(false)
    }
}
```

### 步骤 2：修改 `Visit<'_> for DepNames`
```rust
impl Visit<'_> for DepNames {
    fn visit_path(&mut self, path: &syn::Path) {
        if let Some(ident) = path.segments.last() {
            let name = ident.ident.to_string();
            if !name.starts_with("_c2rust_private_") {
                self.mark_used(name);  // 标记为 used，但不覆盖 pub 标记
            }
        }
        visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &syn::Macro) {
        self.mac_tokens.push_str(&mac.tokens.to_string());
    }
}
```

### 步骤 3：创建统一的 `PubDepVisitor`
```rust
struct PubDepVisitor<'a>(&'a mut HashMap<String, bool>);

impl Visit<'_> for PubDepVisitor<'_> {
    fn visit_path(&mut self, path: &syn::Path) {
        if let Some(ident) = path.segments.last() {
            let name = ident.ident.to_string();
            if !name.starts_with("_c2rust_private_") {
                // 直接覆盖，将间接依赖提升为 pub
                self.0.insert(name, true);
            }
        }
        visit_path(self, path);
    }

    // 跳过函数体、表达式、语句（用于签名访问时）
    fn visit_block(&mut self, _: &syn::Block) {}
    fn visit_expr(&mut self, _: &syn::Expr) {}
    fn visit_stmt(&mut self, _: &syn::Stmt) {}
}
```

### 步骤 4：修改 `parse_rust_file` 方法
```rust
if let Some(syn::Item::Fn(fn_item)) = main_item {
    // 如果是 pub 函数，先访问签名，标记 pub 依赖
    if matches!(fn_item.vis, syn::Visibility::Public(_)) {
        PubDepVisitor(&mut deps.used_names).visit_signature(&fn_item.sig);
    }
    // 访问整个函数，标记所有依赖
    visit_item(deps, &syn::Item::Fn(fn_item.clone()));
    all_items.push(syn::Item::Fn(fn_item));
} else if let Some(syn::Item::Static(var_item)) = main_item {
    // 如果是 pub 静态变量，访问类型，标记 pub 依赖
    if matches!(var_item.vis, syn::Visibility::Public(_)) {
        PubDepVisitor(&mut deps.used_names).visit_type(&var_item.ty);
    }
    // 访问整个静态变量，标记所有依赖
    visit_item(deps, &syn::Item::Static(var_item.clone()));
    all_items.push(syn::Item::Static(var_item));
}
```

### 步骤 5：修改 `filter_dependencies` 方法
```rust
fn filter_dependencies(
    mut all_types: HashMap<String, syn::Item>,
    all_ffi: HashMap<String, syn::ForeignItem>,
    deps: &mut DepNames,
    dep_types: &mut Vec<syn::Item>,
    dep_ffi: &mut Vec<syn::ForeignItem>,
) {
    // 第一步：处理 FFI 依赖
    for (name, item) in all_ffi {
        if deps.contains(&name) {
            visit_foreign_item(deps, &item);
            dep_ffi.push(item);
        }
    }

    // 第二步：收集类型依赖，同时传播 pub 标记
    let mut new_dep = true;
    while new_dep {
        new_dep = false;
        all_types.retain(|name, item| {
            if deps.contains(name) {
                // 如果是 pub 依赖，收集其内部依赖
                if deps.is_pub(name) {
                    PubDepVisitor(&mut deps.used_names).visit_item(item);
                }

                visit_item(deps, item);
                dep_types.push(item.clone());
                new_dep = true;
                return false;
            }
            true
        });
    }

    dep_types.sort_by(|a, b| Self::item_name(b).cmp(&Self::item_name(a)));
    dep_ffi.sort_by(|a, b| Self::foreign_item_name(b).cmp(&Self::foreign_item_name(a)));
}
```

### 步骤 6：修改 `merge_file` 中的类型可见性处理
```rust
for type_item in &type_items {
    let mut type_item = type_item.clone();

    if let Some(name) = Self::item_name(&type_item) {
        // 如果类型不是 pub 依赖，设置为 pub(crate)
        if !deps.is_pub(&name) {
            Self::set_item_visibility(&mut type_item, syn::Visibility::Public(
                syn::Path::from(syn::Ident::new("crate", proc_macro2::Span::call_site()))
            ));
        }

        // 添加 impl 块
        if let Some(impl_blocks) = impl_map.get(&name) {
            for impl_block in impl_blocks {
                merged_items.push(syn::Item::Impl(impl_block.clone()));
            }
        }
    }

    merged_items.push(type_item);
}
```

### 步骤 7：新增辅助方法 `set_item_visibility`
```rust
fn set_item_visibility(item: &mut syn::Item, vis: syn::Visibility) {
    match item {
        syn::Item::Struct(s) => s.vis = vis.clone(),
        syn::Item::Union(u) => u.vis = vis.clone(),
        syn::Item::Const(c) => c.vis = vis.clone(),
        syn::Item::Type(t) => t.vis = vis.clone(),
        _ => {}
    }
}
```

## 预期的测试用例

### 测试用例 1：基本间接依赖
**输入 mod.rs**：
```rust
pub struct InnerType { x: i32 }
pub struct OuterType { inner: InnerType }

pub fn pub_func(a: OuterType) -> i32 { 0 }
```
**预期合并结果**：
```rust
pub struct InnerType { x: i32 }
pub struct OuterType { inner: InnerType }

pub fn pub_func(a: OuterType) -> i32 { 0 }
```

### 测试用例 2：仅在函数体中使用（降级为 pub(crate)）
**输入 mod.rs**：
```rust
pub struct OnlyInBody { x: i32 }

pub fn pub_func() -> i32 {
    let _ = OnlyInBody { x: 1 };
    0
}
```
**预期合并结果**：
```rust
pub(crate) struct OnlyInBody { x: i32 }

pub fn pub_func() -> i32 {
    let _ = OnlyInBody { x: 1 };
    0
}
```

### 测试用例 3：间接依赖覆盖原有标记（关键测试）
**输入 mod.rs**：
```rust
pub struct Inner { x: i32 }
pub struct Outer { inner: Inner }

pub fn pub_func(a: Outer) -> i32 {
    // Inner 既被 pub 签名间接依赖，又在函数体中使用
    let _ = Inner { x: 1 };
    0
}
```
**预期合并结果**：
```rust
pub struct Inner { x: i32 }  // 虽然在函数体中使用，但被 PubDepVisitor 覆盖为 pub
pub struct Outer { inner: Inner }

pub fn pub_func(a: Outer) -> i32 {
    let _ = Inner { x: 1 };
    0
}
```

### 测试用例 4：多层间接依赖
**输入 mod.rs**：
```rust
pub struct Level1 { x: i32 }
pub struct Level2 { l1: Level1 }
pub struct Level3 { l2: Level2 }

pub fn pub_func(a: Level3) -> i32 { 0 }
```
**预期合并结果**：
```rust
pub struct Level1 { x: i32 }
pub struct Level2 { l1: Level1 }
pub struct Level3 { l2: Level2 }

pub fn pub_func(a: Level3) -> i32 { 0 }
```

### 测试用例 5：混合场景
**输入 mod.rs**：
```rust
pub struct A { x: i32 }
pub struct B { y: i32 }
pub struct C { z: i32 }
pub struct D { a: A, b: B }

pub fn pub_func1(d: D) -> i32 { 0 }
fn private_func(c: C) -> i32 { 0 }
```
**预期合并结果**：
```rust
pub struct A { x: i32 }
pub struct B { y: i32 }
pub(crate) struct C { z: i32 }
pub struct D { a: A, b: B }

pub fn pub_func1(d: D) -> i32 { 0 }
fn private_func(c: C) -> i32 { 0 }
```

## 实现细节总结

1. **关键修正**：
   - `PubDepVisitor` 使用 `insert(name, true)` 直接覆盖
   - 将仅在函数体中使用的类型（`false`）提升为 pub（`true`），如果它是 pub 签名的间接依赖

2. **修改点**：
   - `DepNames` 结构体及其方法
   - `parse_rust_file` 方法（使用 `PubDepVisitor`）
   - `filter_dependencies` 方法（使用 `PubDepVisitor`）
   - `merge_file` 方法（调整可见性）
   - 新增 `PubDepVisitor`
   - 新增 `set_item_visibility` 辅助方法

3. **逻辑流程**：
   - `parse_rust_file`：访问 pub 函数/静态变量签名 → `PubDepVisitor` 标记为 `true`
   - 访问整个函数/静态变量 → `DepNames` 标记为 `false`（不覆盖 `true`）
   - `filter_dependencies`：如果是 pub 类型 → `PubDepVisitor` 收集内部依赖，覆盖为 `true`

4. **优化点**：
   - 合并为单个 `PubDepVisitor`，代码最简洁
   - `used_names: HashMap<String, bool>` 存储所有依赖和 pub 标记
   - `insert(name, true)` 直接覆盖，实现间接依赖提升