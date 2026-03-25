# 重构 Feature::merge 实现方案

## 目标概述
将类型定义及其 impl 块整合为一个新的包装类型 `TypeItem`，并重构整个 merge 流程以支持这一新的数据结构，使类型和 impl 块作为一个整体进行处理。

## 涉及的文件和模块
- **主要文件**: `src/merge.rs` - 包含所有 merge 相关逻辑

## 需求
1. 定义新的包装类型 `TypeItem(syn::Item, Vec<syn::ItemImpl>)`，表示一个类型以及这个类型对应的多个 ItemImpl，他们是一个整体进行处理
2. 其他所有原来表示类型 Item 的参数 `syn::Item` 都更改为 `Vec<TypeItem>`
3. TypeItem 去重的时候，不比较 ItemImpl 是否相同
4. TypeItem 写入文件时，类型 Item 和 ItemImpl 一起写入
5. ItemImpl 会参与依赖关系的解析，但绝对不参与 pub 类型依赖关系解析

## 技术方案

### 一、数据结构定义

#### 1. TypeItem 结构体（新增）
```rust
struct TypeItem {
    type_def: syn::Item,           // 类型定义（Struct/Union/Const/Type）
    impl_blocks: Vec<syn::ItemImpl>, // 对应的所有 impl 块
}

impl TypeItem {
    fn new(type_def: syn::Item) -> Self {
        Self {
            type_def,
            impl_blocks: Vec::new(),
        }
    }

    fn name(&self) -> Option<String> {
        Feature::item_name(&self.type_def)
    }

    fn add_impl(&mut self, impl_block: syn::ItemImpl) {
        self.impl_blocks.push(impl_block);
    }
}

impl Clone for TypeItem {
    fn clone(&self) -> Self {
        Self {
            type_def: self.type_def.clone(),
            impl_blocks: self.impl_blocks.clone(),
        }
    }
}
```

#### 2. CollectedItems 结构体（修改）
```rust
struct CollectedItems {
    named_items: HashMap<String, Vec<TypeItem>>,  // 改为 TypeItem
    ffi_items: HashMap<String, Vec<syn::ForeignItem>>,
    foreign_mod_template: Option<syn::ItemForeignMod>,
}
```

#### 3. Duplicates 结构体（修改）
```rust
struct Duplicates {
    named_to_extract: Vec<TypeItem>,  // 改为 TypeItem
    named_remove_set: HashSet<String>,
    ffi_to_extract: Vec<syn::ForeignItem>,
    ffi_remove_set: HashSet<String>,
}
```

---

### 二、类型收集逻辑

#### 1. collect_items_from_files 修改
```rust
fn collect_items_from_files(mod_files: &[PathBuf]) -> Result<CollectedItems> {
    let mut named_items: HashMap<String, Vec<TypeItem>> = HashMap::new();
    let mut ffi_items: HashMap<String, Vec<syn::ForeignItem>> = HashMap::new();
    let mut foreign_mod_template: Option<syn::ItemForeignMod> = None;

    for mod_file in mod_files {
        let content = fs::read_to_string(mod_file).log_err(...)?;
        let ast = syn::parse_file(&content).log_err(...)?;

        // 单遍遍历：同时收集类型定义和 impl 块
        for item in ast.items {
            match item {
                syn::Item::Struct(s) => {
                    let name = s.ident.to_string();
                    named_items.entry(name).or_default()
                        .push(TypeItem::new(syn::Item::Struct(s)));
                }
                syn::Item::Union(u) => {
                    let name = u.ident.to_string();
                    named_items.entry(name).or_default()
                        .push(TypeItem::new(syn::Item::Union(u)));
                }
                syn::Item::Const(c) => {
                    let name = c.ident.to_string();
                    named_items.entry(name).or_default()
                        .push(TypeItem::new(syn::Item::Const(c)));
                }
                syn::Item::Type(t) => {
                    let name = t.ident.to_string();
                    named_items.entry(name).or_default()
                        .push(TypeItem::new(syn::Item::Type(t)));
                }
                syn::Item::Impl(impl_block) => {
                    // 如果类型已存在，添加 impl；否则忽略
                    if let Some(type_name) = Self::impl_self_type_name(&impl_block) {
                        if let Some(type_items) = named_items.get_mut(&type_name) {
                            // 将 impl 添加到所有同名 TypeItem
                            for type_item in type_items {
                                type_item.add_impl(impl_block.clone());
                            }
                        }
                        // 如果类型不存在，impl 块被静默忽略
                    }
                }
                syn::Item::ForeignMod(fm) => {
                    if foreign_mod_template.is_none() {
                        let mut template = fm.clone();
                        template.items.clear();
                        foreign_mod_template = Some(template);
                    }
                    for ffi_item in fm.items {
                        let name = Self::ffi_name(&ffi_item);
                        ffi_items.entry(name).or_default().push(ffi_item);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(CollectedItems {
        named_items,
        ffi_items,
        foreign_mod_template,
    })
}
```

#### 2. extract_dependencies 修改
```rust
fn extract_dependencies(
    mod_rs: &Path,
    deps: &mut DepNames,
) -> Result<(Vec<TypeItem>, Option<syn::ItemForeignMod>)> {
    let content = fs::read_to_string(mod_rs).log_err(...)?;
    let ast = syn::parse_file(&content).log_err(...)?;

    let mut type_map: HashMap<String, TypeItem> = HashMap::new();
    let mut all_ffi: HashMap<String, syn::ForeignItem> = HashMap::new();
    let mut foreign_mod_template: Option<syn::ItemForeignMod> = None;

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
            syn::Item::Impl(impl_block) => {
                // 如果类型已存在，添加 impl；否则忽略
                if let Some(type_name) = Self::impl_self_type_name(&impl_block) {
                    if let Some(type_item) = type_map.get_mut(&type_name) {
                        type_item.add_impl(impl_block);
                    }
                    // 如果类型不存在，impl 块被静默忽略
                }
            }
            _ => {
                // 类型定义处理
                if let Some(name) = Self::item_name(&item) {
                    type_map.insert(name, TypeItem::new(item));
                }
            }
        }
    }

    // 添加内置类型
    type_map.insert("c_size_t".to_string(), TypeItem::new(
        syn::parse_str("type c_size_t = usize;").unwrap()
    ));
    type_map.insert("c_ssize_t".to_string(), TypeItem::new(
        syn::parse_str("type c_ssize_t = isize;").unwrap()
    ));
    type_map.insert("c_ptrdiff_t".to_string(), TypeItem::new(
        syn::parse_str("type c_ptrdiff_t = isize;").unwrap()
    ));

    let mut dep_types = Vec::new();
    let mut dep_ffi = Vec::new();
    Self::filter_dependencies(type_map, all_ffi, deps, &mut dep_types, &mut dep_ffi);

    Ok((dep_types, foreign_mod))
}
```

---

### 三、依赖解析逻辑

#### filter_dependencies 修改
```rust
fn filter_dependencies(
    mut all_types: HashMap<String, TypeItem>,  // 改为 TypeItem
    all_ffi: HashMap<String, syn::ForeignItem>,
    deps: &mut DepNames,
    dep_types: &mut Vec<TypeItem>,  // 改为 TypeItem
    dep_ffi: &mut Vec<syn::ForeignItem>,
) {
    for (name, item) in all_ffi {
        if deps.contains(&name) {
            visit_foreign_item(deps, &item);
            dep_ffi.push(item);
        }
    }

    let mut new_dep = true;
    while new_dep {
        new_dep = false;
        all_types.retain(|name, type_item| {
            if deps.contains(name) {
                // 普通依赖：访问类型定义 + 所有 impl 块
                visit_item(deps, &type_item.type_def);
                for impl_block in &type_item.impl_blocks {
                    visit_item(deps, &syn::Item::Impl(impl_block.clone()));
                }

                // pub 依赖：只访问类型定义的签名，跳过 impl 块
                if deps.is_pub(name) {
                    PubDepVisitor(deps).visit_item(&type_item.type_def);
                    // 不访问 impl 块，因为 pub 依赖不包含 impl
                }

                dep_types.push(type_item.clone());
                new_dep = true;
                return false;
            }
            true
        });
    }

    dep_types.sort_by(|a, b| a.name().cmp(&b.name()));
    dep_ffi.sort_by(|a, b| Self::foreign_item_name(b).cmp(&Self::foreign_item_name(a)));
}
```

**关键设计点**：
- **普通依赖**：`visit_item(deps, &syn::Item::Impl(...))` 完整遍历 impl 块，识别其中引用的类型
- **pub 依赖**：`PubDepVisitor` 只访问 `type_def`，不访问 impl 块

---

### 四、去重逻辑

#### find_duplicates 修改
```rust
fn find_duplicates(
    named_items: &HashMap<String, Vec<TypeItem>>,
    ffi_items: &HashMap<String, Vec<syn::ForeignItem>>,
) -> Duplicates {
    let mut named_to_extract: Vec<TypeItem> = Vec::new();
    let mut named_remove_set: HashSet<String> = HashSet::new();
    let mut ffi_to_extract: Vec<syn::ForeignItem> = Vec::new();
    let mut ffi_remove_set: HashSet<String> = HashSet::new();

    // 类型去重：只比较 type_def，不比较 impl_blocks
    for (type_name, type_items) in named_items {
        if type_items.len() > 1 {
            // 获取第一个类型定义的规范化内容
            let first_type_body = Self::item_body(&type_items[0].type_def);

            // 检查所有类型定义是否相同
            if type_items
                .iter()
                .all(|type_item| Self::item_body(&type_item.type_def) == first_type_body)
            {
                // 类型定义相同，视为重复
                // 提取第一个 TypeItem（包含其 impl_blocks）
                // 注意：不合并其他 TypeItem 的 impl_blocks
                named_to_extract.push(type_items[0].clone());
                named_remove_set.insert(type_name.clone());
            }
        }
    }

    // FFI 去重
    for (name, items) in ffi_items {
        if items.len() > 1 {
            ffi_to_extract.push(items[0].clone());
            ffi_remove_set.insert(name.clone());
        }
    }

    Duplicates {
        named_to_extract,
        named_remove_set,
        ffi_to_extract,
        ffi_remove_set,
    }
}
```

**关键设计点**：
- 只比较 `type_def` 的规范化内容（使用 `item_body()`）
- 不比较 `impl_blocks`，即使 impl 块不同也视为重复
- 只提取第一个 `TypeItem`，不合并其他 `TypeItem` 的 `impl_blocks`

---

### 五、文件写入逻辑

#### 1. generate_lib_rs 修改
```rust
fn generate_lib_rs(
    src_2: &Path,
    mod_files: &[PathBuf],
    duplicates: &Duplicates,
    foreign_mod_template: &Option<syn::ItemForeignMod>,
) -> Result<()> {
    let mut lib_items: Vec<syn::Item> = Vec::new();

    // 写入重复的类型及其所有 impl 块
    for type_item in &duplicates.named_to_extract {
        // 先写入类型定义
        lib_items.push(type_item.type_def.clone());
        // 再写入所有 impl 块
        for impl_block in &type_item.impl_blocks {
            lib_items.push(syn::Item::Impl(impl_block.clone()));
        }
    }

    // 写入重复的 FFI
    if !duplicates.ffi_to_extract.is_empty() {
        if let Some(mut fm) = foreign_mod_template.clone() {
            fm.items = duplicates.ffi_to_extract.clone();
            lib_items.push(syn::Item::ForeignMod(fm));
        }
    }

    let attrs: Vec<syn::Attribute> = syn::Attribute::parse_inner
        .parse_str(Self::lib_attrs())
        .unwrap_or_default();

    let mut lib_file = syn::File {
        shebang: None,
        attrs,
        items: vec![syn::parse_str("use ::core::ffi::*;").unwrap()],
    };

    lib_file.items.extend(lib_items);

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

#### 2. remove_duplicates_from_files 修改
```rust
fn remove_duplicates_from_files(mod_files: &[PathBuf], duplicates: &Duplicates) -> Result<()> {
    for mod_file in mod_files {
        let content =
            fs::read_to_string(mod_file).log_err(&format!("read {}", mod_file.display()))?;
        let mut ast =
            syn::parse_file(&content).log_err(&format!("parse {}", mod_file.display()))?;

        ast.items.retain_mut(|item| match item {
            // 移除重复的类型定义及其 impl 块
            syn::Item::Struct(s) => !duplicates.named_remove_set.contains(&s.ident.to_string()),
            syn::Item::Union(u) => !duplicates.named_remove_set.contains(&u.ident.to_string()),
            syn::Item::Const(c) => !duplicates.named_remove_set.contains(&c.ident.to_string()),
            syn::Item::Type(t) => !duplicates.named_remove_set.contains(&t.ident.to_string()),

            // 移除重复类型的 impl 块
            syn::Item::Impl(impl_block) => {
                if let Some(type_name) = Self::impl_self_type_name(&impl_block) {
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

## 实施步骤

1. 定义 `TypeItem` 结构体及相关方法
2. 修改 `CollectedItems` 和 `Duplicates` 数据结构
3. 修改 `collect_items_from_files` 以收集和关联 impl 块
4. 修改 `extract_dependencies` 以收集 impl 块并返回 `Vec<TypeItem>`
5. 修改 `filter_dependencies` 以处理 `TypeItem` 并实现依赖解析规则
6. 修改 `find_duplicates` 以比较类型定义而非完整 `TypeItem`
7. 修改 `generate_lib_rs` 以写入类型和 impl
8. 修改 `remove_duplicates_from_files` 以移除重复类型及其 impl
9. 更新所有使用 `syn::Item` 表示类型的地方改为使用 `TypeItem`
10. 编写并运行测试用例
11. 运行 `cargo test` 验证所有测试通过

---

## 测试用例

### 测试用例1: TypeItem 基本功能
```rust
#[test]
fn test_typeitem_basic() {
    let type_def: syn::Item = syn::parse_str("struct MyStruct { x: i32 }").unwrap();
    let mut type_item = TypeItem::new(type_def.clone());

    assert_eq!(type_item.name(), Some("MyStruct".to_string()));
    assert_eq!(type_item.impl_blocks.len(), 0);

    let impl_block: syn::ItemImpl = syn::parse_str("impl MyStruct { fn new() -> Self { Self { x: 0 } } }").unwrap();
    type_item.add_impl(impl_block);

    assert_eq!(type_item.impl_blocks.len(), 1);
}
```

### 测试用例2: impl 块参与依赖解析但不参与 pub 依赖解析
```rust
#[test]
fn test_impl_dependency_rules() {
    // 普通依赖：impl 块中引用的类型应被识别为依赖
    let mut deps = DepNames::new();
    let impl_block: syn::ItemImpl = syn::parse_str("impl MyStruct { fn get_other(&self) -> OtherType { ... } }").unwrap();
    visit_item(&mut deps, &syn::Item::Impl(impl_block));
    assert!(deps.contains("OtherType")); // ✓ 普通依赖

    // pub 依赖：impl 块不应影响 pub 依赖标记
    let mut pub_deps = DepNames::new();
    let impl_block: syn::ItemImpl = syn::parse_str("impl MyStruct { fn method(&self) -> PubType { ... } }").unwrap();
    PubDepVisitor(&mut pub_deps).visit_item(&syn::Item::Impl(impl_block));
    assert!(!pub_deps.is_pub("PubType")); // ✗ 不标记为 pub 依赖
}
```

### 测试用例3: 去重时不比较 impl
```rust
#[test]
fn test_deduplicate_not_merge_impls() {
    // 模块 A
    let type_def_a: syn::Item = syn::parse_str("struct Point { x: i32, y: i32 }").unwrap();
    let impl_a1: syn::ItemImpl = syn::parse_str("impl Point { fn new(x: i32, y: i32) -> Self { Self { x, y } } }").unwrap();
    let impl_a2: syn::ItemImpl = syn::parse_str("impl Point { fn get_x(&self) -> i32 { self.x } }").unwrap();

    let mut type_item_a = TypeItem::new(type_def_a.clone());
    type_item_a.add_impl(impl_a1.clone());
    type_item_a.add_impl(impl_a2.clone());

    // 模块 B - 相同类型定义，不同 impl
    let type_def_b: syn::Item = syn::parse_str("struct Point { x: i32, y: i32 }").unwrap();
    let impl_b1: syn::ItemImpl = syn::parse_str("impl Point { fn get_y(&self) -> i32 { self.y } }").unwrap();

    let mut type_item_b = TypeItem::new(type_def_b.clone());
    type_item_b.add_impl(impl_b1.clone());

    // 模拟去重
    let mut named_items: HashMap<String, Vec<TypeItem>> = HashMap::new();
    named_items.insert("Point".to_string(), vec![type_item_a, type_item_b]);

    let duplicates = Feature::find_duplicates(&named_items, &HashMap::new());

    // 验证
    assert!(duplicates.named_remove_set.contains("Point"));
    assert_eq!(duplicates.named_to_extract.len(), 1);

    let extracted_type_item = &duplicates.named_to_extract[0];
    assert_eq!(extracted_type_item.name(), Some("Point".to_string()));
    // 只有第一个 TypeItem 的 impl 块被提取，不合并其他 TypeItem 的 impl
    assert_eq!(extracted_type_item.impl_blocks.len(), 2); // impl_a1 + impl_a2
}
```

### 测试用例4: collect_items_from_files 正确关联 impl 块
```rust
#[test]
fn test_collect_items_from_files_with_impls() {
    // 创建临时文件
    let temp_dir = tempfile::tempdir().unwrap();
    let mod_file = temp_dir.path().join("mod_test.rs");

    let content = r#"
        struct Point { x: i32, y: i32 }
        impl Point {
            fn new(x: i32, y: i32) -> Self {
                Self { x, y }
            }
        }
        struct Counter { count: i32 }
        impl Counter {
            fn increment(&mut self) {
                self.count += 1;
            }
        }
    "#;

    fs::write(&mod_file, content).unwrap();

    // 收集 items
    let collected = Feature::collect_items_from_files(&vec![mod_file]).unwrap();

    // 验证 Point
    assert_eq!(collected.named_items.len(), 2);
    assert!(collected.named_items.contains_key("Point"));

    let point_items = &collected.named_items["Point"];
    assert_eq!(point_items.len(), 1);
    assert_eq!(point_items[0].name(), Some("Point".to_string()));
    assert_eq!(point_items[0].impl_blocks.len(), 1);

    // 验证 Counter
    assert!(collected.named_items.contains_key("Counter"));
    let counter_items = &collected.named_items["Counter"];
    assert_eq!(counter_items.len(), 1);
    assert_eq!(counter_items[0].impl_blocks.len(), 1);
}
```

### 测试用例5: impl 块在类型定义之前被忽略
```rust
#[test]
fn test_impl_before_type_definition_is_ignored() {
    let content = r#"
        impl Point {
            fn new(x: i32, y: i32) -> Self {
                Self { x, y }
            }
        }
        struct Point { x: i32, y: i32 }
    "#;

    let ast: syn::File = syn::parse_str(content).unwrap();

    let mut type_map: HashMap<String, TypeItem> = HashMap::new();

    // 单遍遍历
    for item in ast.items {
        match item {
            syn::Item::Impl(impl_block) => {
                if let Some(type_name) = Feature::impl_self_type_name(&impl_block) {
                    // 此时 Point 还不存在，impl 被忽略
                    if let Some(type_item) = type_map.get_mut(&type_name) {
                        type_item.add_impl(impl_block);
                    }
                }
            }
            _ => {
                if let Some(name) = Feature::item_name(&item) {
                    type_map.insert(name, TypeItem::new(item));
                }
            }
        }
    }

    // Point 存在，但没有 impl 块
    assert!(type_map.contains_key("Point"));
    assert_eq!(type_map["Point"].impl_blocks.len(), 0);
}
```

### 测试用例6: generate_lib_rs 正确写入类型和 impl
```rust
#[test]
fn test_generate_lib_rs_with_impls() {
    let temp_dir = tempfile::tempdir().unwrap();
    let src_2 = temp_dir.path().join("src.2");
    fs::create_dir_all(&src_2).unwrap();

    // 创建重复的类型
    let type_def: syn::Item = syn::parse_str("struct Counter { count: i32 }").unwrap();
    let impl1: syn::ItemImpl = syn::parse_str("impl Counter { fn new() -> Self { Self { count: 0 } } }").unwrap();
    let impl2: syn::ItemImpl = syn::parse_str("impl Counter { fn increment(&mut self) { self.count += 1; } }").unwrap();

    let mut type_item = TypeItem::new(type_def);
    type_item.add_impl(impl1);
    type_item.add_impl(impl2);

    let duplicates = Duplicates {
        named_to_extract: vec![type_item],
        named_remove_set: std::collections::HashSet::new(),
        ffi_to_extract: vec![],
        ffi_remove_set: std::collections::HashSet::new(),
    };

    let mod_file = src_2.join("mod_test.rs");
    fs::write(&mod_file, "pub fn test() {}").unwrap();

    let result = Feature::generate_lib_rs(&src_2, &vec![mod_file], &duplicates, &None);
    assert!(result.is_ok());

    let lib_rs_path = src_2.join("lib.rs");
    let lib_content = fs::read_to_string(&lib_rs_path).unwrap();

    // 验证内容
    assert!(lib_content.contains("struct Counter"));
    assert!(lib_content.contains("impl Counter { fn new()"));
    assert!(lib_content.contains("impl Counter { fn increment(&mut self)"));
    assert!(lib_content.contains("mod mod_test;"));
}
```

### 测试用例7: remove_duplicates_from_files 正确移除类型和 impl
```rust
#[test]
fn test_remove_duplicates_from_files_with_impls() {
    let temp_dir = tempfile::tempdir().unwrap();
    let src_2 = temp_dir.path().join("src.2");
    fs::create_dir_all(&src_2).unwrap();

    let mod_file = src_2.join("mod_test.rs");
    let content = r#"
        struct Point { x: i32, y: i32 }
        impl Point {
            fn new(x: i32, y: i32) -> Self {
                Self { x, y }
            }
        }
        struct KeepMe { value: i32 }
    "#;
    fs::write(&mod_file, content).unwrap();

    let duplicates = Duplicates {
        named_to_extract: vec![],
        named_remove_set: {
            let mut set = std::collections::HashSet::new();
            set.insert("Point".to_string());
            set
        },
        ffi_to_extract: vec![],
        ffi_remove_set: std::collections::HashSet::new(),
    };

    let result = Feature::remove_duplicates_from_files(&vec![mod_file.clone()], &duplicates);
    assert!(result.is_ok());

    let result_content = fs::read_to_string(&mod_file).unwrap();

    // Point 及其 impl 应该被移除
    assert!(!result_content.contains("struct Point"));
    assert!(!result_content.contains("impl Point"));
    // KeepMe 应该保留
    assert!(result_content.contains("struct KeepMe"));
}
```

### 测试用例8: impl 块中的类型被识别为普通依赖
```rust
#[test]
fn test_impl_contributes_to_normal_dependencies() {
    let mut deps = DepNames::new();

    // 创建包含 impl 的代码
    let code = r#"
        struct Container { data: i32 }
        impl Container {
            fn process(&self, other: &Helper) -> i32 {
                other.get_value() + self.data
            }
        }
        struct Helper { value: i32 }
        impl Helper {
            fn get_value(&self) -> i32 {
                self.value
            }
        }
    "#;

    let ast: syn::File = syn::parse_str(code).unwrap();

    // 模拟: Container 被使用，触发依赖解析
    deps.mark_used("Container".to_string());

    // 执行依赖过滤（简化版）
    let mut all_types: HashMap<String, TypeItem> = HashMap::new();
    for item in ast.items {
        if let syn::Item::Struct(s) = item {
            all_types.insert(s.ident.to_string(), TypeItem::new(syn::Item::Struct(s)));
        } else if let syn::Item::Impl(impl_block) = item {
            if let Some(type_name) = Feature::impl_self_type_name(&impl_block) {
                if let Some(type_item) = all_types.get_mut(&type_name) {
                    type_item.add_impl(impl_block);
                }
            }
        }
    }

    let mut dep_types = Vec::new();
    filter_dependencies(all_types, HashMap::new(), &mut deps, &mut dep_types, &mut vec![]);

    // Container 被使用，且其 impl 中引用了 Helper
    // 因此 Helper 也应该被识别为依赖
    let type_names: Vec<_> = dep_types.iter()
        .filter_map(|t| t.name())
        .collect();

    assert!(type_names.contains(&"Container".to_string()));
    assert!(type_names.contains(&"Helper".to_string()));
}
```

### 测试用例9: impl 块不贡献 pub 依赖
```rust
#[test]
fn test_impl_does_not_contribute_to_pub_dependencies() {
    let mut deps = DepNames::new();

    // 创建 pub 类型及其 impl
    let code = r#"
        pub struct Point { x: i32, y: i32 }
        impl Point {
            pub fn distance_to(&self, other: &Point) -> f64 {
                // 实现
                0.0
            }
            pub fn to_string(&self) -> String {
                format!("({}, {})", self.x, self.y)
            }
        }
    "#;

    let ast: syn::File = syn::parse_str(code).unwrap();

    // 模拟: Point 是 pub 依赖
    deps.mark_pub("Point".to_string());

    // 执行 pub 依赖解析
    let mut all_types: HashMap<String, TypeItem> = HashMap::new();
    for item in ast.items {
        if let syn::Item::Struct(s) = item {
            all_types.insert(s.ident.to_string(), TypeItem::new(syn::Item::Struct(s)));
        } else if let syn::Item::Impl(impl_block) = item {
            if let Some(type_name) = Feature::impl_self_type_name(&impl_block) {
                if let Some(type_item) = all_types.get_mut(&type_name) {
                    type_item.add_impl(impl_block);
                }
            }
        }
    }

    // 使用 PubDepVisitor
    let mut dep_types = Vec::new();
    let mut filtered_types = all_types.clone();
    filtered_types.retain(|name, type_item| {
        if deps.is_pub(name) {
            // 只访问 type_def，不访问 impl 块
            PubDepVisitor(&mut deps).visit_item(&type_item.type_def);
            // 不执行: for impl_block in &type_item.impl_blocks
            dep_types.push(type_item.clone());
            return false;
        }
        true
    });

    // 只有 Point 应该被识别为 pub 依赖
    let type_names: Vec<_> = dep_types.iter()
        .filter_map(|t| t.name())
        .collect();

    assert_eq!(type_names, vec!["Point"]);
    // String 不应该被标记为 pub 依赖，因为它只出现在 impl 的实现中
    assert!(!deps.is_pub("String"));
}
```

---

## 变更总结表

| 变更点 | 修改位置 | 修改内容 | 目的 |
|-------|---------|---------|------|
| 1 | **新增** | `TypeItem` 结构体及方法 | 包装类型定义和 impl 块 |
| 2 | `CollectedItems` | `named_items` 改为 `HashMap<String, Vec<TypeItem>>` | 支持 TypeItem |
| 3 | `Duplicates` | `named_to_extract` 改为 `Vec<TypeItem>` | 提取 TypeItem 到 lib.rs |
| 4 | `extract_dependencies()` | 返回类型改为 `Vec<TypeItem>` | 支持 TypeItem |
| 5 | `extract_dependencies()` | 添加 `Item::Impl` 处理 | 收集 impl 块 |
| 6 | `collect_items_from_files()` | 返回类型改为 `CollectedItems` (TypeItem) | 收集 TypeItem |
| 7 | `collect_items_from_files()` | 添加 `Item::Impl` 处理 | 收集 impl 块 |
| 8 | `filter_dependencies()` | 参数改为 `TypeItem` | 支持 TypeItem |
| 9 | `filter_dependencies()` | 普通依赖中访问 impl 块 | impl 参与普通依赖解析 |
| 10 | `filter_dependencies()` | pub 依赖中跳过 impl 块 | impl 不参与 pub 依赖解析 |
| 11 | `find_duplicates()` | 参数改为 `TypeItem` | 处理 TypeItem |
| 12 | `find_duplicates()` | 只比较 type_def，提取第一个 TypeItem | 去重时不比较 impl |
| 13 | `generate_lib_rs()` | 写入 TypeItem 的 type_def 和 impl_blocks | 类型与 impl 一起写入 |
| 14 | `remove_duplicates_from_files()` | 移除重复类型及其 impl 块 | 完整清理重复项 |