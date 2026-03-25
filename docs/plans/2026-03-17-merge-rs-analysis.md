# merge.rs 完整实现逻辑分析

## 文档目的

本文档详细分析 `merge.rs` 的实现逻辑，提供足够的信息以便在不依赖原始代码的情况下进行重构设计。

## 概述

`merge.rs` 实现了 Rust 代码的合并和去重功能，主要目的是：
1. 将每个 mod_xxx 目录下的 fun_*.rs、var_*.rs 文件和 mod.rs 合并为单个 mod_xxx.rs 文件
2. 在合并文件中仅保留该模块实际依赖的类型和 FFI 声明
3. 将重复的类型、impl 块和 FFI 声明提取到 lib.rs
4. 从各个模块文件中删除已提取到 lib.rs 的重复内容

## 核心数据流

```
rust/src/mod_xxx/
├── mod.rs (类型定义、FFI 声明、impl 块)
├── fun_foo.rs (函数实现 + 辅助类型)
└── fun_bar.rs (函数实现 + 辅助类型)
         ↓
   merge_file() (合并单个文件)
         ↓
rust/src.2/mod_xxx.rs (合并后的文件)
         ↓
   deduplicate_mod_rs() (去重)
         ↓
rust/src.2/lib.rs (公共类型和 FFI)
rust/src.2/mod_xxx.rs (删除重复后的文件)
```

## 核心数据结构

### 1. DepNames (依赖名称跟踪器)

**位置**: lines 12-70

**定义**:
```rust
struct DepNames {
    used_names: HashMap<String, bool>, // bool = true 表示 pub 依赖，false 表示非 pub
    mac_tokens: String,               // 用于在 macro 中匹配依赖
}
```

**字段说明**:
- `used_names`: 跟踪代码中使用的类型/函数名称
  - Key: 类型或函数名称
  - Value: `true` = pub 签名依赖，`false` = 非 pub 依赖（函数体中使用）
- `mac_tokens`: 累积 macro 的 token 字符串，用于正则匹配 macro 中的依赖

**方法**:
- `new()`: 创建空实例
- `contains(name)`: 检查 name 是否在依赖中（包括 macro 中的匹配）
- `mark_used(name)`: 标记 name 为非 pub 依赖（如果不存在则插入 false）
- `mark_pub(name)`: 标记 name 为 pub 依赖（插入 true）
- `is_pub(name)`: 查询 name 是否为 pub 依赖

**Visitor 实现**:
- `visit_path`: 遍历类型路径，提取最后一个 segment 的名称（跳过 _c2rust_private_ 前缀）
- `visit_use_name`: 处理 use 语句中的名称
- `visit_use_rename`: 处理 use 语句中的重命名
- `visit_macro`: 累积 macro 的 token 字符串

**关键行为**:
1. 遍历 AST 时自动收集所有类型引用
2. 使用正则 `[^a-zA-Z_]{name}[^a-zA-Z_]` 在 macro tokens 中匹配名称
3. 跳过 `_c2rust_private_` 前缀的名称（这些是内部符号）

### 2. PubDepVisitor (pub 依赖访问器)

**位置**: lines 72-88

**定义**:
```rust
struct PubDepVisitor<'a>(&'a mut DepNames);
```

**用途**: 专门用于遍历 pub 签名（函数签名、静态变量类型），将引用的类型标记为 pub 依赖

**Visitor 实现**:
- `visit_path`: 与 DepNames 相同，但调用 `mark_pub` 而非 `mark_used`
- `visit_block`: **空实现**（不遍历函数体）
- `visit_expr`: **空实现**（不遍历表达式）
- `visit_stmt`: **空实现**（不遍历语句）

**关键行为**:
1. 只遍历 pub 签名的类型引用，不进入函数体
2. 所有找到的依赖都标记为 pub（true）
3. 用于区分 pub 签名依赖和函数体内部依赖

### 3. CollectedItems (收集的所有项)

**位置**: lines 90-95

**定义**:
```rust
struct CollectedItems {
    named_items: HashMap<String, Vec<syn::Item>>,       // 按名称组织的类型定义
    ffi_items: HashMap<String, Vec<syn::ForeignItem>>,  // 按名称组织的 FFI 声明
    impl_items: HashMap<String, Vec<syn::ItemImpl>>,    // 按类型名称组织的 impl 块
    foreign_mod_template: Option<syn::ItemForeignMod>,  // FFI 模块模板
}
```

**字段说明**:
- `named_items`: Key = 类型名称，Value = 该类型在所有文件中的定义（可能重复）
- `ffi_items`: Key = FFI 函数名（考虑 link_name），Value = 该 FFI 在所有文件中的声明
- `impl_items`: Key = impl 的自类型名称，Value = 该类型的所有 impl 块
- `foreign_mod_template`: 从第一个文件中提取的 extern "C" 模板

**关键行为**:
1. 从所有合并后的文件中收集所有类型、FFI 和 impl 块
2. 按名称分组，便于后续去重比较
3. impl 块按自类型名称分组（不是按 impl 块本身的名称）

### 4. Duplicates (重复项信息)

**位置**: lines 97-103

**定义**:
```rust
struct Duplicates {
    named_to_extract: Vec<syn::Item>,           // 需要提取到 lib.rs 的类型定义
    named_remove_set: HashSet<String>,         // 需要从模块文件中删除的类型名称
    impl_to_extract: Vec<syn::ItemImpl>,       // 需要提取到 lib.rs 的 impl 块
    ffi_to_extract: Vec<syn::ForeignItem>,     // 需要提取到 lib.rs 的 FFI 声明
    ffi_remove_set: HashSet<String>,           // 需要从模块文件中删除的 FFI 名称
}
```

**字段说明**:
- `named_to_extract`: 提取到 lib.rs 的类型定义（每个类型只保留一份）
- `named_remove_set`: 这些类型在模块文件中需要删除
- `impl_to_extract`: 提取到 lib.rs 的 impl 块（已去重）
- `ffi_to_extract`: 提取到 lib.rs 的 FFI 声明（每个 FFI 只保留一份）
- `ffi_remove_set`: 这些 FFI 在模块文件中需要删除

**关键行为**:
1. 标识哪些类型/FFI/impl 需要移动到 lib.rs
2. 记录需要从模块文件中删除的名称集合

## 核心函数分析

### 1. Feature::merge

**位置**: lines 109-122

**签名**:
```rust
pub fn merge(&mut self) -> Result<()>
```

**流程**:
1. 遍历所有 files，对每个文件调用 `merge_file()`
2. 调用 `deduplicate_mod_rs()` 去重并提取到 lib.rs
3. 调用 `link_src()` 切换 src 目录链接

**输出**:
- 在 `rust/src.2/` 目录生成合并后的所有模块文件和 lib.rs

### 2. merge_file

**位置**: lines 125-223

**签名**:
```rust
fn merge_file(&self, file: &File) -> Result<bool>
```

**流程**:

#### 2.1 收集模块名
- 调用 `collect_modules_from_mod_rs()` 从 mod.rs 中提取 fun_*/var_* 模块名

#### 2.2 解析 Rust 文件并收集依赖
- 对每个模块文件：
  - 调用 `parse_rust_file()` 解析文件
  - 提取主函数/变量实现
  - 收集代码中使用的所有类型/函数依赖（通过 DepNames）

#### 2.3 提取 mod.rs 中的依赖
- 调用 `extract_dependencies(mod_rs, deps)`
  - 返回：`(Vec<Item>, Vec<ItemImpl>, Option<ItemForeignMod>)`
  - 第一个 Vec: 依赖的类型定义（按 pub 需求设置可见性）
  - 第二个 Vec: 所有 impl 块（未过滤）
  - Option: 依赖的 FFI 声明

#### 2.4 过滤 impl 块
- 遍历所有 impl 块，只保留自类型在 `dep_type_names` 中的 impl
- 构建类型到 impl 列表的映射 `impl_map`

#### 2.5 生成合并文件
- 文件开头添加 `use super::*;`
- 添加别名：`use super::{mod_name} as {alias};` (对每个模块名)
- 添加类型定义：
  - 对每个类型，根据 `deps.is_pub(type_name)` 设置可见性
  - 在类型定义后添加该类型对应的 impl 块
- 添加 FFI 声明（如果有）
- 添加函数/变量实现

#### 2.6 写入文件
- 输出到 `rust/src.2/{mod_name}.rs`

**返回值**: `true` = 成功合并，`false` = 无需合并

**关键点**:
- impl 块只保留其自类型被依赖的 impl
- impl 块的可见性与类型相同（不单独控制）
- 类型可见性由是否在 pub 签名中使用决定

### 3. collect_modules_from_mod_rs

**位置**: lines 225-253

**签名**:
```rust
fn collect_modules_from_mod_rs(mod_dir: &Path) -> Result<Vec<String>>
```

**流程**:
1. 读取 `mod_dir/mod.rs`
2. 解析 AST
3. 遍历 items，提取 `mod fun_*` 和 `mod var_*` 模块名
4. 检查对应的 .rs 文件是否存在
5. 返回模块名列表

**返回值**: 模块名列表（如 `["fun_foo", "var_bar"]`）

### 4. parse_rust_file

**位置**: lines 255-317

**签名**:
```rust
fn parse_rust_file(rs_file: &Path, all_items: &mut Vec<syn::Item>, deps: &mut DepNames) -> Result<()>
```

**流程**:

#### 4.1 文件名解析
- 从文件名提取主项名称（如 `fun_foo.rs` → `foo`）

#### 4.2 AST 遍历
- 查找与文件名匹配的主函数/变量
- 其他项（辅助类型、use 语句等）收集到 `other_items`

#### 4.3 处理主函数
- 读取对应的 `decl_foo.rs` 文件（包含 FFI 签名）
- 调用 `merge_item_fn()` 替换函数签名
- 如果函数是 pub，用 `PubDepVisitor` 遍历签名（标记 pub 依赖）
- 用 `DepNames` 遍历整个函数（标记所有依赖）
- 添加到 `all_items`

#### 4.4 处理主变量
- 调用 `merge_item_static()` 合并辅助项
- 如果变量是 pub，用 `PubDepVisitor` 遍历类型
- 用 `DepNames` 遍历整个变量
- 添加到 `all_items`

**关键点**:
- 区分 pub 签名依赖和非 pub 依赖
- 使用 decl 文件修正函数签名（与 FFI 对齐）

### 5. merge_item_fn

**位置**: lines 345-369

**签名**:
```rust
fn merge_item_fn(items: Vec<syn::Item>, fn_item: &mut syn::ItemFn, decl_file: &Path) -> Result<()>
```

**流程**:
1. 如果有 `_c2rust_private_` 属性，移除并设置可见性为 Inherited
2. 读取 `decl_file` 中的 FFI 签名
3. 交换函数签名（用 decl 文件的签名替换）
   - 只交换参数类型和返回值类型
   - 不交换参数名
4. 将 `items`（辅助类型定义）插入到函数体前

**关键点**:
- 确保函数签名与 FFI 声明一致
- 辅助类型定义保持在同一个文件中

### 6. merge_item_static

**位置**: lines 371-384

**签名**:
```rust
fn merge_item_static(items: Vec<syn::Item>, static_item: &mut syn::ItemStatic) -> Result<()>
```

**流程**:
1. 如果有 `_c2rust_private_` 属性，移除并设置可见性为 Inherited
2. 将 `items` 插入到变量初始化表达式前

### 7. extract_dependencies

**位置**: lines 432-499

**签名**:
```rust
fn extract_dependencies(
    mod_rs: &Path,
    deps: &mut DepNames,
) -> Result<(Vec<syn::Item>, Vec<syn::ItemImpl>, Option<syn::ItemForeignMod>)>
```

**流程**:

#### 7.1 收集所有项
- 读取 `mod_rs` 文件并解析 AST
- 遍历 items：
  - `Item::ForeignMod`: 收集 FFI 声明到 `all_ffi`，保存模板
  - `Item::Impl`: 添加到 `all_impls` 列表
  - 其他命名项：添加到 `all_types` HashMap

#### 7.2 添加内置类型
- 插入 `c_size_t`, `c_ssize_t`, `c_ptrdiff_t`

#### 7.3 过滤依赖
- 调用 `filter_dependencies(all_types, all_ffi, deps, &mut dep_types, &mut dep_ffi)`
  - 根据 `deps` 过滤类型和 FFI
  - 处理传递依赖

#### 7.4 构建 FFI 模块
- 如果有依赖的 FFI，用模板创建 `ItemForeignMod`

**返回值**:
- `Vec<Item>`: 依赖的类型定义（未设置可见性）
- `Vec<ItemImpl>`: 所有 impl 块（未过滤）
- `Option<ItemForeignMod>`: 依赖的 FFI 声明

**关键点**:
- impl 块全部返回，不在此时过滤
- 类型可见性在 `merge_file` 中设置

### 8. filter_dependencies

**位置**: lines 501-534

**签名**:
```rust
fn filter_dependencies(
    mut all_types: HashMap<String, syn::Item>,
    all_ffi: HashMap<String, syn::ForeignItem>,
    deps: &mut DepNames,
    dep_types: &mut Vec<syn::Item>,
    dep_ffi: &mut Vec<syn::ForeignItem>,
)
```

**流程**:

#### 8.1 过滤 FFI
- 遍历 `all_ffi`，如果在 `deps` 中则：
  - 用 `visit_foreign_item` 遍历 FFI（可能引用类型）
  - 添加到 `dep_ffi`

#### 8.2 过滤类型（传递依赖）
- 使用 `while new_dep` 循环处理传递依赖
- 每次迭代：
  - 遍历 `all_types`
  - 如果类型名在 `deps` 中：
    - 如果类型是 pub 依赖，用 `PubDepVisitor` 遍历（标记新的 pub 依赖）
    - 用普通 `visit_item` 遍历（标记新的非 pub 依赖）
    - 添加到 `dep_types`
    - 从 `all_types` 中移除
    - 设置 `new_dep = true`（可能引入新的传递依赖）
  - 如果类型名不在 `deps` 中，保留在 `all_types` 中

#### 8.3 排序
- 按 `item_name` 降序排序

**关键点**:
- 循环直到没有新的依赖被添加
- pub 类型的字段类型也标记为 pub 依赖
- 确保传递依赖被完全解析

### 9. deduplicate_mod_rs

**位置**: lines 559-595

**签名**:
```rust
fn deduplicate_mod_rs(&self) -> Result<()>
```

**流程**:

#### 9.1 收集所有模块文件
- 调用 `collect_mod_files()` 获取 `src.2` 下所有 `mod_*.rs` 文件

#### 9.2 收集所有项
- 调用 `collect_items_from_files()` 从所有模块文件中收集 items
- 返回 `CollectedItems`

#### 9.3 找出重复项
- 调用 `find_duplicates()` 识别重复的类型、impl 和 FFI
- 返回 `Duplicates`

#### 9.4 生成 lib.rs
- 调用 `generate_lib_rs()` 写入 lib.rs

#### 9.5 删除重复项
- 如果有需要删除的项，调用 `remove_duplicates_from_files()`

### 10. collect_mod_files

**位置**: lines 597-614

**签名**:
```rust
fn collect_mod_files(src_2: &Path) -> Result<Vec<PathBuf>>
```

**流程**:
1. 遍历 `src_2` 目录（深度 1）
2. 过滤出：
   - 是文件
   - 扩展名为 .rs
   - 文件名以 `mod_` 开头
3. 返回文件路径列表

### 11. collect_items_from_files

**位置**: lines 616-685

**签名**:
```rust
fn collect_items_from_files(mod_files: &[PathBuf]) -> Result<CollectedItems>
```

**流程**:
1. 初始化空的 `CollectedItems`
2. 遍历每个 `mod_file`：
   - 读取并解析 AST
   - 遍历 items：
     - `Item::Struct`: 按 ident 分组到 `named_items`
     - `Item::Union`: 按 ident 分组到 `named_items`
     - `Item::Const`: 按 ident 分组到 `named_items`
     - `Item::Type`: 按 ident 分组到 `named_items`
     - `Item::Impl`: 按 `impl_self_type_name()` 分组到 `impl_items`
     - `Item::ForeignMod`: 收集 FFI 到 `ffi_items`，保存模板
3. 返回 `CollectedItems`

**关键点**:
- impl 块按自类型名称分组（不是 impl 块本身的名称）
- 所有文件的相同类型都在同一个 Vec 中

### 12. find_duplicates

**位置**: lines 701-752

**签名**:
```rust
fn find_duplicates(
    named_items: &HashMap<String, Vec<syn::Item>>,
    impl_items: &HashMap<String, Vec<syn::ItemImpl>>,
    ffi_items: &HashMap<String, Vec<syn::ForeignItem>>,
) -> Duplicates
```

**流程**:

#### 12.1 类型去重
- 遍历 `named_items`：
  - 如果 Vec 为空，跳过
  - 获取第一个类型的 `item_body()`（清除属性和可见性后的字符串）
  - 如果所有类型的 `item_body()` 都相同：
    - 将第一个类型添加到 `named_to_extract`
    - 将类型名添加到 `named_remove_set`
    - **收集该类型的所有 impl 块**（无论内容是否相同）
      - 从 `impl_items` 获取该类型的所有 impl
      - 使用 HashSet 去重（按 impl 的 token 字符串）
      - 添加到 `impl_to_extract`

#### 12.2 FFI 去重
- 遍历 `ffi_items`：
  - 如果 Vec 不为空：
    - 将第一个 FFI 添加到 `ffi_to_extract`
    - 将 FFI 名添加到 `ffi_remove_set`
  - **注意**：FFI 不比较内容，只要名称相同就视为重复

#### 12.3 返回
- 构造 `Duplicates` 结构并返回

**关键点**:
- 类型比较使用 `item_body()`（排除属性和可见性）
- impl 块收集**所有文件中的所有 impl**（跨文件聚合）
- impl 块去重使用 token 字符串比较
- FFI 不比较内容，只按名称去重

**当前问题**:
- impl 块收集了所有文件中的所有 impl，而不是单个文件中的 impl
- 如果某个类型在多个文件中有不同的 impl，所有 impl 都会被提取

### 13. generate_lib_rs

**位置**: lines 754-808

**签名**:
```rust
fn generate_lib_rs(
    src_2: &Path,
    mod_files: &[PathBuf],
    duplicates: &Duplicates,
    foreign_mod_template: &Option<syn::ItemForeignMod>,
) -> Result<()>
```

**流程**:

#### 13.1 收集 lib.rs 的 items
- 初始化空 Vec

#### 13.2 添加类型定义
- 扩展 `duplicates.named_to_extract` 到 `lib_items`

#### 13.3 添加 impl 块
- 遍历每个提取的类型：
  - 遍历 `duplicates.impl_to_extract`：
    - 如果 impl 的自类型与当前类型匹配，添加到 `lib_items`

#### 13.4 添加 FFI 声明
- 如果有 FFI，使用模板创建 `ItemForeignMod` 并添加

#### 13.5 构建 lib.rs 文件
- 解析库属性（`lib_attrs()`）
- 创建 `syn::File`：
  - 添加 `use ::core::ffi::*;`
  - 添加所有 `lib_items`
  - 添加所有模块声明：`mod {mod_name};`

#### 13.6 写入文件
- 格式化并写入 `src_2/lib.rs`

**关键点**:
- impl 块通过类型名匹配（impl 块可能比类型定义多）
- FFI 统一放在一个 extern "C" 模块中
- 模块声明放在最后

### 14. remove_duplicates_from_files

**位置**: lines 810-842

**签名**:
```rust
fn remove_duplicates_from_files(mod_files: &[PathBuf], duplicates: &Duplicates) -> Result<()>
```

**流程**:

#### 14.1 遍历每个模块文件
- 读取并解析 AST

#### 14.2 过滤 items
- 使用 `retain_mut` 保留不在删除集中的 items：

  - `Item::Struct`: ident 不在 `named_remove_set` 中
  - `Item::Union`: ident 不在 `named_remove_set` 中
  - `Item::Const`: ident 不在 `named_remove_set` 中
  - `Item::Type`: ident 不在 `named_remove_set` 中
  - `Item::Impl`: 自类型不在 `named_remove_set` 中
  - `Item::ForeignMod`:
    - 保留 FFI 不在 `ffi_remove_set` 中的
    - 如果所有 FFI 都被删除，删除整个 ForeignMod
  - 其他：保留

#### 14.3 写入文件
- 格式化并覆盖原文件

**关键点**:
- impl 块通过自类型判断是否删除
- 如果 ForeignMod 的所有 FFI 都被删除，整个 ForeignMod 会被删除

## 辅助函数

### item_name

**位置**: lines 386-394

**签名**:
```rust
fn item_name(item: &syn::Item) -> Option<String>
```

**功能**: 提取命名项的名称

**返回值**:
- `Item::Struct`: Some(ident)
- `Item::Union`: Some(ident)
- `Item::Const`: Some(ident)
- `Item::Type`: Some(ident)
- 其他: None

**当前限制**: 不支持 `Item::Impl`

### foreign_item_name

**位置**: lines 396-402

**签名**:
```rust
fn foreign_item_name(item: &syn::ForeignItem) -> Option<String>
```

**功能**: 提取 FFI 项的名称

**返回值**:
- `ForeignItem::Fn`: Some(sig.ident)
- `ForeignItem::Static`: Some(ident)
- 其他: None

### impl_self_type_name

**位置**: lines 404-415

**签名**:
```rust
fn impl_self_type_name(impl_item: &syn::ItemImpl) -> Option<String>
```

**功能**: 提取 impl 块的自类型名称

**流程**:
1. 检查 `self_ty` 是否为 `Type::Path` 且无 qself
2. 提取路径的最后一个 segment 的 ident

**返回值**: 自类型名称或 None

### set_item_visibility

**位置**: lines 417-430

**签名**:
```rust
fn set_item_visibility(item: &mut syn::Item, is_pub: bool)
```

**功能**: 设置项的可见性

**行为**:
- 如果 `is_pub`: 设置为 `pub`
- 否则: 设置为 `Inherited`

**支持的项**:
- `Item::Struct`
- `Item::Union`
- `Item::Const`
- `Item::Type`

### item_body

**位置**: lines 687-699

**签名**:
```rust
fn item_body(item: &syn::Item) -> String
```

**功能**: 获取项的主体（清除属性和可见性）

**流程**:
1. 克隆 item
2. 清除属性
3. 设置可见性为 `Inherited`
4. 返回 token 字符串

**用途**: 用于类型去重比较（排除属性和可见性差异）

### ffi_name

**位置**: lines 844-854

**签名**:
```rust
fn ffi_name(item: &syn::ForeignItem) -> String
```

**功能**: 获取 FFI 项的名称（考虑 link_name）

**流程**:
- `ForeignItem::Fn`: 优先提取 link_name，否则使用 sig.ident
- `ForeignItem::Static`: 优先提取 link_name，否则使用 ident
- 其他: 返回空字符串

### extract_link_name

**位置**: lines 856-872

**签名**:
```rust
fn extract_link_name(attrs: &[syn::Attribute]) -> Option<String>
```

**功能**: 从属性中提取 link_name

**流程**:
1. 遍历属性
2. 查找包含 "link_name" 的属性
3. 提取引号中的值

**返回值**: link_name 值或 None

### link_src

**位置**: lines 536-548

**签名**:
```rust
fn link_src(&self) -> Result<()>
```

**功能**: 切换 src 目录链接

**流程**:
1. 如果 `rust/src` 是符号链接，删除它
2. 否则，重命名 `rust/src` → `rust/src.1`
3. 创建符号链接 `rust/src` → `rust/src.2`

### remove_private_attr

**位置**: lines 550-557

**签名**:
```rust
fn remove_private_attr(attrs: &mut Vec<syn::Attribute>) -> bool
```

**功能**: 移除包含 `_c2rust_private_` 的属性

**返回值**: 是否移除了属性

## 依赖关系图

```
Feature::merge
├── merge_file
│   ├── collect_modules_from_mod_rs
│   ├── parse_rust_file
│   │   ├── merge_item_fn
│   │   │   ├── remove_private_attr
│   │   │   └── merge_fn_signature
│   │   ├── merge_item_static
│   │   │   └── remove_private_attr
│   │   ├── is_use_super
│   │   └── item_name
│   ├── extract_dependencies
│   │   ├── item_name
│   │   ├── foreign_item_name
│   │   ├── filter_dependencies
│   │   └── impl_self_type_name
│   ├── impl_self_type_name
│   ├── set_item_visibility
│   └── item_name
├── deduplicate_mod_rs
│   ├── collect_mod_files
│   ├── collect_items_from_files
│   │   ├── item_name
│   │   ├── impl_self_type_name
│   │   └── ffi_name
│   ├── find_duplicates
│   │   ├── item_body
│   │   ├── item_name
│   │   └── impl_self_type_name
│   ├── generate_lib_rs
│   │   ├── item_name
│   │   └── impl_self_type_name
│   └── remove_duplicates_from_files
│       └── impl_self_type_name
└── link_src
```

## 当前实现的问题和限制

### 1. ItemImpl 的处理问题

**问题**: impl 块在多个地方被重复处理

**现状**:
- `merge_file` 中过滤 impl 块（lines 164-179）
- `extract_dependencies` 返回所有 impl 块（不在此处过滤）
- `collect_items_from_files` 重新收集所有 impl 块
- `find_duplicates` 跨文件收集所有 impl 块

**问题细节**:
1. `merge_file` 过滤 impl 块：只保留自类型被依赖的 impl
2. `find_duplicates` 收集 impl 块：收集所有文件中该类型的所有 impl
3. 结果：如果某个类型在文件 A 和文件 B 中都有 impl，两个文件的 impl 都会被提取到 lib.rs

### 2. ItemImpl 的可见性问题

**问题**: impl 块没有独立的 pub/non-pub 依赖跟踪

**现状**:
- 类型的 pub/non-pub 由 DepNames 跟踪
- impl 块没有可见性标记
- impl 块的可见性跟随其自类型

**问题细节**:
1. impl 块中使用的类型依赖没有被跟踪
2. 如果 impl 块引用了其他类型，这些类型不会被标记为依赖
3. 可能导致某些必要的类型被错误删除

### 3. ItemImpl 的去重策略问题

**问题**: impl 块的去重策略不明确

**现状**:
- `find_duplicates` 收集所有文件中的所有 impl 块
- 使用 HashSet 按 token 字符串去重
- 不管 impl 块是否相同，全部收集

**问题细节**:
1. 如果同一个类型在不同文件中有不同的 impl，所有 impl 都会被保留
2. 这可能导致 lib.rs 中包含过多 impl 块
3. 无法控制选择哪个文件的 impl

### 4. ItemImpl 的文件来源追踪缺失

**问题**: 无法知道 impl 块来自哪个文件

**现状**:
- `CollectedItems.impl_items` 只按类型名称分组
- 不记录 impl 块来自哪个文件

**问题细节**:
1. 无法选择单个文件中的所有 impl 块
2. 无法实现"保存某个类型在任何一个文件中的全部 ItemImpl"的需求

## 关键设计决策

### 1. 为什么分两阶段？

**阶段 1: merge_file**
- 目的：合并单个 C 文件对应的多个 Rust 文件
- 保留：函数/变量实现 + 依赖的类型 + 依赖的 FFI
- 过滤：impl 块只保留其自类型被依赖的 impl

**阶段 2: deduplicate_mod_rs**
- 目的：跨文件去重，提取公共定义到 lib.rs
- 比较：类型定义的内容（排除属性和可见性）
- 提取：重复的类型及其所有 impl 块

### 2. 为什么使用 token 字符串比较？

**原因**:
- 需要比较 AST 的语义内容
- 排除属性和可见性的差异
- 简单可靠

**实现**: `item_body()` 函数清除属性和可见性后转换为字符串

### 3. 为什么 impl 块全部提取？

**原因**:
- impl 块的内容可能在不同文件中不同
- 即使类型定义相同，impl 可能不同
- 保守策略：全部保留

**问题**: 可能导致 lib.rs 过大

### 4. 为什么使用 DepNames 而不是直接遍历？

**原因**:
- 需要区分 pub 签名依赖和非 pub 依赖
- 需要处理传递依赖
- 需要在 macro 中匹配名称

**实现**:
- `PubDepVisitor` 只遍历签名，标记 pub 依赖
- 普通 `DepNames` 遍历整个代码，标记所有依赖

## 重构建议

### 建议 1: 统一 ItemImpl 的处理位置

**当前**: `merge_file` 和 `deduplicate_mod_rs` 都处理 impl 块

**建议**: 只在 `deduplicate_mod_rs` 中处理 impl 块

**理由**:
1. 简化逻辑，避免重复处理
2. impl 块的去重和提取应该在全局范围内进行

### 建议 2: 支持单个文件的 impl 块选择

**当前**: 收集所有文件的所有 impl 块

**建议**: 修改 `CollectedItems.impl_items` 的数据结构

**新结构**:
```rust
struct CollectedItems {
    impl_items: HashMap<String, HashMap<PathBuf, Vec<syn::ItemImpl>>>,
    // 类型名称 -> 文件路径 -> impl 块列表
}
```

**理由**:
1. 可以追踪每个 impl 块的来源文件
2. 可以选择单个文件中的所有 impl 块
3. 支持更灵活的 impl 块选择策略

### 建议 3: 跟踪 impl 块的类型依赖

**当前**: impl 块的类型依赖没有被跟踪

**建议**: 在 `filter_dependencies` 中处理 impl 块的依赖

**实现**:
```rust
if deps.contains(name) {
    // ... 处理类型本身的依赖
    // 新增：处理该类型的 impl 块依赖
    if let Some(impls) = all_impls_by_type.get(name) {
        for impl_item in impls {
            // impl 块的依赖都是非 pub 的
            visit_item(deps, &syn::Item::Impl(impl_item.clone()));
        }
    }
}
```

**理由**:
1. 确保 impl 块引用的类型被正确标记为依赖
2. 避免删除必要的类型

### 建议 4: 扩展 item_name 支持 ItemImpl

**当前**: `item_name` 不支持 `Item::Impl`

**建议**: 添加支持

**实现**:
```rust
fn item_name(item: &syn::Item) -> Option<String> {
    match item {
        syn::Item::Struct(item) => Some(item.ident.to_string()),
        syn::Item::Union(item) => Some(item.ident.to_string()),
        syn::Item::Const(item) => Some(item.ident.to_string()),
        syn::Item::Type(item) => Some(item.ident.to_string()),
        syn::Item::Impl(item) => Self::impl_self_type_name(item),
        _ => None,
    }
}
```

**理由**:
1. 统一接口
2. 简化代码

## 总结

`merge.rs` 实现了一个复杂的代码合并和去重系统，主要挑战在于：

1. **依赖跟踪**: 区分 pub 签名依赖和非 pub 依赖，处理传递依赖
2. **impl 块处理**: impl 块没有独立的可见性，需要特殊处理
3. **去重策略**: 需要跨文件比较类型定义，提取公共定义
4. **文件结构**: 需要维护合并前后文件的对应关系

当前实现的主要问题集中在 impl 块的处理上，需要重构以支持更精细的控制。