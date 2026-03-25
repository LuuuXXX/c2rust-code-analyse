# 扩展 remove_duplicate_items 功能以支持 ItemStruct/Union/Type/Const 去重

## 目标概述
扩展 `Feature::remove_duplicate_items` 功能，使其不仅删除重复的 FFI 定义（ForeignItem::Fn, ForeignItem::Static），还能删除重复定义的 ItemStruct, ItemUnion, ItemType, ItemConst。

## 涉及的文件和模块
- **主要文件**：`/home/dududingding/h00339793/rust/C2RustXW-CLI/c2rust-code-analyse/src/feature.rs`
- **参考文件**：`/home/dududingding/h00339793/rust/C2RustXW-CLI/c2rust-code-analyse/src/merge.rs`（已有的去重逻辑）

## 技术选型与修改思路

### 核心逻辑
参考现有的 `remove_duplicate_items` 函数的双 Visitor 模式：
1. **Read-only Visitor（Visit trait）**：从 mod.rs 文件收集需要保留的 item 名称
2. **Mutable Visitor（VisitMut trait）**：遍历目标文件，删除重复项

### 修改方案

#### 1. 扩展 Visit trait 的 Visitor 结构体
在 `remove_duplicate_items` 函数中的 `Visitor` 结构体，新增以下方法：

```rust
impl Visit<'_> for Visitor {
    // 现有方法
    fn visit_foreign_item_fn(&mut self, f: &syn::ForeignItemFn) {
        self.0.insert(f.sig.ident.to_string());
    }
    fn visit_foreign_item_static(&mut self, v: &syn::ForeignItemStatic) {
        self.0.insert(v.ident.to_string());
    }

    // 新增方法
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
```

#### 2. 扩展 VisitMut trait 的删除逻辑
在 `visit_file_mut` 调用后，添加对顶层 items 的过滤：

```rust
impl VisitMut for Visitor {
    fn visit_item_foreign_mod_mut(&mut self, m: &mut syn::ItemForeignMod) {
        // 现有逻辑保持不变
        m.items.retain(|item| match item {
            syn::ForeignItem::Fn(f) => !self.0.contains(&f.sig.ident.to_string()),
            syn::ForeignItem::Static(v) => !self.0.contains(&v.ident.to_string()),
            _ => true,
        });
    }

    fn visit_item_mut(&mut self, i: &mut syn::Item) {
        // 先处理子节点
        syn::visit_mut::visit_item_mut(self, i);

        // 然后判断是否需要需要删除
        let should_remove = match i {
            syn::Item::Struct(s) => self.0.contains(&s.ident.to_string()),
            syn::Item::Union(u) => self.0.contains(&u.ident.to_string()),
            syn::Item::Type(t) => self.0.contains(&t.ident.to_string()),
            syn::Item::Const(c) => self.0.contains(&c.ident.to_string()),
            _ => false,
        };

        if should_remove {
            // 设置为空 Item 来标记删除
            *i = syn::Item::Verbatim(quote::quote!());
        }
    }
}
```

#### 3. 在 visit_file_mut 后清理空 Item
由于 `visit_item_mut` 不能直接从列表中删除，需要在 `visit_file_mut` 调用后添加清理步骤：

```rust
visit_file_mut(&mut ffi_names, ast);

// 清理空的 Item（被标记删除的）
ast.items.retain(|item| {
    !matches!(item, syn::Item::Verbatim(t) if t.to_string().trim().is_empty())
});
```

## 预期的测试用例

### 测试用例 1：结构体去重
**场景**：mod.rs 中定义了 `struct MyStruct { x: i32 }`，目标文件中也定义了相同的 `struct MyStruct`
**预期**：目标文件中的 `MyStruct` 定义被删除

### 测试用例 2：类型别名去重
**场景**：mod.rs 中定义了 `type MyType = i32;`，目标文件中也定义了相同的 `type MyType = i32;`
**预期**：目标文件中的 `type MyType` 定义被删除

### 测试用例 3：混合去重
**场景**：mod.rs 中定义了结构体、联合体、类型别名、常量、静态变量，目标文件中均有对应定义
**预期**：所有5种类型的重复定义都被正确删除

### 测试用例 4：不同定义不被删除
**场景**：mod.rs 中定义了 `struct MyStruct { x: i32 }`，目标文件中定义了 `struct MyStruct { x: f64 }`（内容不同）
**预期**：由于是按名称匹配，目标文件中的定义仍然会被删除（这是当前逻辑的行为）

## 实施计划

1. 修改 `src/feature.rs` 中的 `remove_duplicate_items` 函数
2. 扩展 Visit trait 添加 4 个新方法
3. 扩展 VisitMut trait 添加 `visit_item_mut` 方法
4. 在 `visit_file_mut` 后添加清理逻辑
5. 编写测试用例验证功能
