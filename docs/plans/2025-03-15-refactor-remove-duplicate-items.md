# 重构 remove_duplicate_items 避免重复解析 mod.rs

## 问题分析

**当前问题：**
- `remove_duplicate_items` 每次被调用时都会重新读取和解析 `mod.rs` 文件
- 调用链：`Feature::update` → 循环每个模块 → 循环每个节点 → `validate_file` → `remove_duplicate_items`
- 如果一个模块有 N 个函数/变量需要验证，`mod.rs` 会被解析 N 次

**性能影响：**
- 模块 `mod_a` 有 10 个函数 → `mod_a/mod.rs` 被解析 10 次
- 模块 `mod_b` 有 5 个函数 → `mod_b/mod.rs` 被解析 5 次

## 重构方案

### 核心思路
在 `Feature::update` 的外层循环中，**每个模块只解析一次 mod.rs**，然后将解析结果传递给 `validate_file` 和 `remove_duplicate_items` 使用。

### 具体修改步骤

**步骤 1：提取独立函数 `collect_item_names_from_mod`**
```rust
/// 从 mod.rs 文件中收集所有需要去重的 Item 名称
fn collect_item_names_from_mod(mod_rs: &Path) -> Result<HashSet<String>> {
    let content = fs::read_to_string(mod_rs).log_err(&format!("read {}", mod_rs.display()))?;
    let mod_ast = syn::parse_file(&content).log_err(&format!("parse {}", mod_rs.display()))?;

    struct Visitor(HashSet<String>);
    impl Visit<'_> for Visitor {
        fn visit_foreign_item_fn(&mut self, f: &syn::ForeignItemFn) {
            self.0.insert(f.sig.ident.to_string());
        }
        fn visit_foreign_item_static(&mut self, v: &syn::ForeignItemStatic) {
            self.0.insert(v.ident.to_string());
        }
        fn visit_item_struct(&mut self, i: &syn::ItemStruct) {
            self.0.insert(i.ident.to_string());
        }
        fn visit_item_union(&mut self, i: &syn::ItemUnion) {
            self.0.insert(i.ident.to_string());
        }
        fn visit_item_type(&mut self, i: &syn::ItemType) {
            self.0.insert(i.ident.to_string());
        }
        fn visit_item_const(&mut self, i: &syn::ItemConst) {
            self.0.insert(i.ident.to_string());
        }
    }

    let mut visitor = Visitor(HashSet::new());
    visit_file(&mut visitor, &mod_ast);
    Ok(visitor.0)
}
```

**步骤 2：修改 `remove_duplicate_items` 函数签名和实现**
```rust
// 修改前
fn remove_duplicate_items(ast: &mut syn::File, mod_rs: &Path) -> Result<()> {
    let content = fs::read_to_string(mod_rs).log_err(...)?;
    let mod_ast = syn::parse_file(&content).log_err(...)?;
    // ... 构建 Visitor 并收集名称
}

// 修改后
fn remove_duplicate_items(ast: &mut syn::File, item_names: &HashSet<String>) -> Result<()> {
    // 不再读取和解析 mod.rs，直接使用传入的 item_names
    let mut visitor = Visitor(item_names.clone());
    visit_file_mut(&mut visitor, ast);
    Ok(())
}

// 辅助结构体移到 remove_duplicate_items 内部或保留为独立实现
struct Visitor(HashSet<String>);
impl VisitMut for Visitor {
    fn visit_item_foreign_mod_mut(&mut self, m: &mut syn::ItemForeignMod) {
        m.items.retain(|item| match item {
            syn::ForeignItem::Fn(f) => !self.0.contains(&f.sig.ident.to_string()),
            syn::ForeignItem::Static(v) => !self.0.contains(&v.ident.to_string()),
            _ => true,
        });
    }

    fn visit_item_mut(&mut self, i: &mut syn::Item) {
        syn::visit_mut::visit_item_mut(self, i);
        let should_remove = match i {
            syn::Item::Struct(s) => self.0.contains(&s.ident.to_string()),
            syn::Item::Union(u) => self.0.contains(&u.ident.to_string()),
            syn::Item::Type(t) => self.0.contains(&t.ident.to_string()),
            syn::Item::Const(c) => self.0.contains(&c.ident.to_string()),
            _ => false,
        };
        if should_remove {
            *i = syn::Item::Verbatim(quote::quote!());
        }
    }
}
```

**步骤 3：修改 `validate_file` 函数签名和调用**
```rust
// 修改前
fn validate_file(file: &Path, link_name: &str, prefix: &str, build_success: bool) -> Result<bool> {
    // ...
    Self::remove_duplicate_items(&mut ast, &file.with_file_name("mod.rs"))?;
    // ...
}

// 修改后
fn validate_file(
    file: &Path,
    link_name: &str,
    prefix: &str,
    build_success: bool,
    item_names: &HashSet<String>,
) -> Result<bool> {
    // ...
    Self::remove_duplicate_items(&mut ast, item_names)?;
    // ...
}
```

**步骤 4：在 `Feature::update` 中外层调用 collect_item_names_from_mod**
```rust
// 在 mod_dir 循环内，node 循环之前
let mod_rs = mod_dir.join("mod.rs");
let item_names = Self::collect_item_names_from_mod(&mod_rs).log_err("collect item names")?;

// 在 node 循环中传递 item_names
for node in file.iter_mut() {
    // ...
    Self::validate_file(&rust_file, &name, &prefixed_name, build_success, &item_names)?;
}
```

**步骤 5：更新测试用例**
```rust
#[test]
fn test_remove_duplicate_items_struct() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    let mod_dir = root.join("rust/src/mod_a");
    fs::create_dir_all(&mod_dir).unwrap();

    let mod_rs = mod_dir.join("mod.rs");
    let target_rs = mod_dir.join("target.rs");

    let mod_content = r#"
struct MyStruct {
    x: i32,
}
"#;
    let target_content = r#"
struct MyStruct {
    x: i32,
}

struct OtherStruct {
    y: f64,
}
"#;

    fs::write(&mod_rs, mod_content).unwrap();
    fs::write(&target_rs, target_content).unwrap();

    let mut ast = syn::parse_file(&fs::read_to_string(&target_rs).unwrap()).unwrap();
    
    // 构建 item_names 集合
    let item_names = Feature::collect_item_names_from_mod(&mod_rs).unwrap();
    
    Feature::remove_duplicate_items(&mut ast, &item_names).unwrap();

    let result = prettyplease::unparse(&ast);
    assert!(!result.contains("struct MyStruct"));
    assert!(result.contains("struct OtherStruct"));
}

// 类似地更新 test_remove_duplicate_items_type 和 test_remove_duplicate_items_mixed
```

## 性能收益

- **优化前**：N 个节点的模块 → mod.rs 解析 N 次
- **优化后**：N 个节点的模块 → mod.rs 解析 1 次
- **收益**：解析次数减少 (N-1)/N，对于大型模块性能提升显著

## 影响范围

**需要修改的文件：**
- `src/feature.rs`:
  - 新增 `collect_item_names_from_mod` 函数
  - 修改 `Feature::update` 函数（外层调用 collect_item_names_from_mod）
  - 修改 `Feature::validate_file` 函数（新增 item_names 参数）
  - 修改 `Feature::remove_duplicate_items` 函数（修改签名和实现）
  - 更新所有相关测试用例（3个）

**重命名说明：**
- 所有相关变量和参数名从 `ffi_names` 更名为 `item_names`
- 原因：现在处理的不仅是 FFI items，还包括 Struct/Union/Type/Const

## 实施计划

1. 新增 `collect_item_names_from_mod` 函数
2. 修改 `remove_duplicate_items` 函数签名和实现
3. 修改 `validate_file` 函数签名和调用
4. 修改 `Feature::update` 函数外层逻辑
5. 更新测试用例
6. 运行测试确保全部通过
7. 提交代码