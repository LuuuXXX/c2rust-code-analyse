# 修复 deduplicate_mod_rs 合并类型时没有合并 impl 语句块

## 目标概述
修复 `deduplicate_mod_rs` 功能，使其在合并重复类型时也能正确处理对应的 impl 语句块。

## 问题分析

1. **问题 1：`deduplicate_mod_rs` 去重时没有处理 impl 块**
   - 当前行为：只比较类型定义，完全忽略 impl 块
   - 正确行为：类型去重时，将所有相关的 impl 块也一起提取到 lib.rs

2. **问题 2：`remove_duplicates_from_files` 移除类型时没有移除对应的 impl 块**
   - 当前行为：当 `MyStruct` 被移到 lib.rs 时，只移除类型定义，但 impl 块仍然保留在原文件中
   - 正确行为：当类型被移到 lib.rs 时，该类型的所有 impl 块也应该从原文件中移除

**核心原则**：
- 类型去重**只比较类型本身**，不比较 impl 块内容
- 当类型被提取到 lib.rs 时，该类型的所有 impl 块（无论内容如何）都一并提取

---

## 涉及的文件和模块
- **主文件**: `src/merge.rs`
  - `CollectedItems` 结构体（第90-94行）- 需要添加 impl_items 字段
  - `Duplicates` 结构体（第97-102行）- 需要添加 impl_to_extract 字段
  - `deduplicate_mod_rs()` 方法（第557-588行）
  - `collect_items_from_files()` 方法（第609-671行）
  - `find_duplicates()` 方法（第687-722行）
  - `generate_lib_rs()` 方法（第724-765行）- 需要输出 impl 块
  - `remove_duplicates_from_files()` 方法（第767-792行）- 需要移除 impl 块

---

## 技术选型或修改思路

### 1. 扩展 `CollectedItems` 结构体
```rust
struct CollectedItems {
    named_items: HashMap<String, Vec<syn::Item>>,
    ffi_items: HashMap<String, Vec<syn::ForeignItem>>,
    impl_items: HashMap<String, Vec<syn::ItemImpl>>,  // 新增：按类型名分组的 impl 块
    foreign_mod_template: Option<syn::ItemForeignMod>,
}
```

### 2. 扩展 `Duplicates` 结构体
```rust
struct Duplicates {
    named_to_extract: Vec<syn::Item>,
    named_remove_set: HashSet<String>,
    impl_to_extract: Vec<syn::ItemImpl>,  // 新增：需要提取到 lib.rs 的 impl 块
    ffi_to_extract: Vec<syn::ForeignItem>,
    ffi_remove_set: HashSet<String>,
}
```

### 3. 修改 `collect_items_from_files()` 收集 impl 块
在第620-663行的 `for item in ast.items` 循环中添加 `syn::Item::Impl` 分支：
```rust
syn::Item::Impl(impl_item) => {
    if let Some(type_name) = Self::impl_self_type_name(&impl_item) {
        impl_items.entry(type_name).or_default().push(impl_item);
    }
}
```

### 4. 修改 `find_duplicates()` 去重逻辑
```rust
fn find_duplicates(
    named_items: &HashMap<String, Vec<syn::Item>>,
    impl_items: &HashMap<String, Vec<syn::ItemImpl>>,
    ffi_items: &HashMap<String, Vec<syn::ForeignItem>>,
) -> Duplicates {
    let mut named_to_extract: Vec<syn::Item> = Vec::new();
    let mut named_remove_set: HashSet<String> = HashSet::new();
    let mut impl_to_extract: Vec<syn::ItemImpl> = Vec::new();
    let mut ffi_to_extract: Vec<syn::ForeignItem> = Vec::new();
    let mut ffi_remove_set: HashSet<String> = HashSet::new();

    // 类型去重（只比较类型本身，不比较 impl 块）
    for (type_name, items) in named_items {
        if items.len() > 0 {
            let first_tokens = Self::item_body(&items[0]);
            if items.iter().all(|item| Self::item_body(item) == first_tokens) {
                named_to_extract.push(items[0].clone());
                named_remove_set.insert(type_name.clone());

                // 收集该类型的所有 impl 块（无论内容是否相同）
                if let Some(impls) = impl_items.get(type_name) {
                    // 去重：每个独特的 impl 只保留一份
                    let mut seen_impls: HashSet<String> = HashSet::new();
                    for impl_item in impls {
                        let impl_tokens = impl_item.to_token_stream().to_string();
                        if seen_impls.insert(impl_tokens) {
                            impl_to_extract.push(impl_item.clone());
                        }
                    }
                }
            }
        }
    }

    // FFI 去重保持不变
    for (name, items) in ffi_items {
        if items.len() > 0 {
            ffi_to_extract.push(items[0].clone());
            ffi_remove_set.insert(name.clone());
        }
    }

    Duplicates {
        named_to_extract,
        named_remove_set,
        impl_to_extract,
        ffi_to_extract,
        ffi_remove_set,
    }
}
```

### 5. 修改 `generate_lib_rs()` 输出 impl 块
在生成 `lib.rs` 时，将提取的 impl 块附加到对应的类型后面：
```rust
fn generate_lib_rs(
    src_2: &Path,
    mod_files: &[PathBuf],
    duplicates: &Duplicates,
    foreign_mod_template: &Option<syn::ItemForeignMod>,
) -> Result<()> {
    let mut lib_items: Vec<syn::Item> = Vec::new();

    // 先添加类型定义
    lib_items.extend(duplicates.named_to_extract.clone());

    // 为每个类型添加对应的 impl 块
    for type_item in &duplicates.named_to_extract {
        if let Some(type_name) = Self::item_name(type_item) {
            for impl_item in &duplicates.impl_to_extract {
                if let Some(impl_type_name) = Self::impl_self_type_name(impl_item) {
                    if impl_type_name == type_name {
                        lib_items.push(syn::Item::Impl(impl_item.clone()));
                    }
                }
            }
        }
    }

    if !duplicates.ffi_to_extract.is_empty() {
        if let Some(mut fm) = foreign_mod_template.clone() {
            fm.items = duplicates.ffi_to_extract.clone();
            lib_items.push(syn::Item::ForeignMod(fm));
        }
    }

    // ... 其余逻辑不变
}
```

### 6. 修改 `remove_duplicates_from_files()` 移除 impl 块
在重写模块文件时，移除被去重类型的所有 impl 块：
```rust
fn remove_duplicates_from_files(mod_files: &[PathBuf], duplicates: &Duplicates) -> Result<()> {
    for mod_file in mod_files {
        let content =
            fs::read_to_string(mod_file).log_err(&format!("read {}", mod_file.display()))?;
        let mut ast =
            syn::parse_file(&content).log_err(&format!("parse {}", mod_file.display()))?;

        ast.items.retain_mut(|item| match item {
            syn::Item::Struct(s) => !duplicates.named_remove_set.contains(&s.ident.to_string()),
            syn::Item::Union(u) => !duplicates.named_remove_set.contains(&u.ident.to_string()),
            syn::Item::Const(c) => !duplicates.named_remove_set.contains(&c.ident.to_string()),
            syn::Item::Type(t) => !duplicates.named_remove_set.contains(&t.ident.to_string()),
            syn::Item::Impl(impl_item) => {
                // 如果 impl 的类型被移除到 lib.rs，则移除该 impl
                if let Some(type_name) = Self::impl_self_type_name(impl_item) {
                    !duplicates.named_remove_set.contains(&type_name)
                } else {
                    true
                }
            }
            syn::Item::ForeignMod(fm) => {
                fm.items
                    .retain(|ffi| !duplicates.ffi_remove_set.contains(&Self::ffi_name(ffi)));
                !fm.items.is_empty()
            }
            _ => true,
        });

        let formatted = prettyplease::unparse(&ast);
        fs::write(mod_file, formatted.as_bytes())
            .log_err(&format!("write {}", mod_file.display()))?;
    }
    Ok(())
}
```

---

## 预期的测试用例

### 测试用例 1：类型相同，impl 块也相同 → 类型去重，impl 块也去重
**场景**：
```rust
// mod_a.rs
struct MyStruct { x: i32 }
impl MyStruct { fn foo(&self) -> i32 { self.x } }

// mod_b.rs
struct MyStruct { x: i32 }
impl MyStruct { fn foo(&self) -> i32 { self.x } }
```
**预期结果**：
- `lib.rs` 包含 `MyStruct` 类型和 `impl MyStruct { fn foo(&self) -> i32 { self.x } }`
- `mod_a.rs` 和 `mod_b.rs` 中的类型和 impl 块都被移除

### 测试用例 2：类型相同，但 impl 块不同 → 类型去重，所有不同的 impl 块都提取到 lib.rs
**场景**：
```rust
// mod_a.rs
struct MyStruct { x: i32 }
impl MyStruct { fn foo(&self) -> i32 { self.x } }

// mod_b.rs
struct MyStruct { x: i32 }
impl MyStruct { fn bar(&self) -> i32 { self.x + 1 } }
```
**预期结果**：
- `lib.rs` 包含 `MyStruct` 类型和两个 impl 块：
  - `impl MyStruct { fn foo(&self) -> i32 { self.x } }`
  - `impl MyStruct { fn bar(&self) -> i32 { self.x + 1 } }`
- `mod_a.rs` 和 `mod_b.rs` 中的类型和 impl 块都被移除

### 测试用例 3：类型相同，只有一个文件有 impl 块 → 类型去重，impl 块提取到 lib.rs
**场景**：
```rust
// mod_a.rs
struct MyStruct { x: i32 }

// mod_b.rs
struct MyStruct { x: i32 }
impl MyStruct { fn foo(&self) -> i32 { self.x } }
```
**预期结果**：
- `lib.rs` 包含 `MyStruct` 类型和 impl 块
- `mod_a.rs` 和 `mod_b.rs` 中的类型被移除
- `mod_b.rs` 中的 impl 块被移除（已提取到 lib.rs）

### 测试用例 4：类型相同且有多个不同的 impl 块 → 类型去重，所有 impl 块提取到 lib.rs
**场景**：
```rust
// mod_a.rs
struct MyStruct { x: i32 }
impl MyStruct { fn foo(&self) -> i32 { self.x } }

// mod_b.rs
struct MyStruct { x: i32 }
impl MyStruct { fn foo(&self) -> i32 { self.x } }
impl MyStruct { fn bar(&self) -> i32 { self.x + 1 } }
```
**预期结果**：
- `lib.rs` 包含 `MyStruct` 类型和两个 impl 块：
  - `impl MyStruct { fn foo(&self) -> i32 { self.x } }`（去重后只保留一份）
  - `impl MyStruct { fn bar(&self) -> i32 { self.x + 1 } }`
- `mod_a.rs` 和 `mod_b.rs` 中的类型和 impl 块都被移除

### 测试用例 5：类型不重复，impl 块不处理
**场景**：
```rust
// mod_a.rs
struct MyStruct { x: i32 }
impl MyStruct { fn foo(&self) -> i32 { self.x } }

// mod_b.rs
struct OtherStruct { y: i32 }
impl OtherStruct { fn bar(&self) -> i32 { self.y } }
```
**预期结果**：
- `lib.rs` 中不包含任何内容
- 两个文件中的类型和 impl 块都保留

### 测试用例 6：类型相同，impl trait 块 → 类型去重，trait impl 也提取
**场景**：
```rust
// mod_a.rs
struct MyStruct { x: i32 }
impl Clone for MyStruct {
    fn clone(&self) -> Self { *self }
}

// mod_b.rs
struct MyStruct { x: i32 }
impl Clone for MyStruct {
    fn clone(&self) -> Self { *self }
}
```
**预期结果**：
- `lib.rs` 包含 `MyStruct` 类型和 `impl Clone for MyStruct` trait impl
- 两个文件中的类型和 impl 块都被移除