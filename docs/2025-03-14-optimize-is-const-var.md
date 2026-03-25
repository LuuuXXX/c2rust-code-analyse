# 优化方案：Kind::is_const_var 使用正则表达式精确匹配 const 变量

## 目标概述
优化 `Kind::is_const_var` 方法，使用正则表达式精确匹配 const 变量，避免误匹配包含 const 子串的关键字，并正确区分顶层 const 和底层 const。

## 涉及的文件和模块
- **主要文件**：`src/file.rs`
- **关键方法**：
  - `Kind::is_const_var`（第 192-197 行）

## 技术选型与修改思路

### 当前实现存在的问题

**位置**：`src/file.rs` 第 192-197 行

**当前代码**：
```rust
pub fn is_const_var(&self) -> bool {
    let Kind::VarDecl(var) = self else {
        return false;
    };
    var.ty.qual_type.starts_with("const ")
}
```

**存在的问题**：
1. 只检查 `qual_type` 是否以 `"const "` 开头，过于简单
2. 没有区分 `const int *` 和 `int * const` 的情况
3. 可能误匹配包含 "const" 子串的关键字（如 `constexpr`, `const_cast` 等）
4. 没有考虑复杂的类型表达式

### 优化目标

根据 C 语言 const 修饰符的语义：
- **顶层 const**：`int * const p` - 指针本身是 const（在 `*` 后面）
- **底层 const**：`const int * p` - 指针指向的对象是 const（在 `*` 前面）
- **普通 const**：`const int x` - 变量本身是 const

**判断逻辑**：
1. 逆向查找是否存在表示指针的 `*`
2. 如果存在 `*`，检查 `*` 之后是否有 `const`（顶层 const）
3. 如果不存在 `*`，检查整个字符串中是否有独立的关键字 `const`
4. 使用 word boundary 正则确保不匹配 `const` 子串

## 具体实现步骤

### 修改 `Kind::is_const_var` 方法

```rust
pub fn is_const_var(&self) -> bool {
    let Kind::VarDecl(var) = self else {
        return false;
    };
    
    let qual_type = &var.ty.qual_type;
    
    // 提前创建正则表达式，避免重复编译
    let const_pattern = regex::Regex::new(r"\bconst\b").unwrap();
    
    // 查找最后一个指针标记 * (从右向左查找，处理多级指针)
    if let Some(last_star_pos) = qual_type.rfind('*') {
        // 检查 * 之后是否有 const（顶层 const）
        const_pattern.is_match(&qual_type[last_star_pos + 1..])
    } else {
        // 没有指针，检查整个类型是否有 const
        const_pattern.is_match(qual_type)
    }
}
```

### 优化说明

**改进点**：
1. **提前创建正则**：在条件表达式之前创建 `const_pattern`，避免在两个分支中重复编译
2. **代码复用**：两个分支都使用同一个 `const_pattern`，提高效率
3. **逻辑清晰**：先准备工具（正则），再进行判断，代码更易读

**正则表达式说明**：
- `\bconst\b` - 使用 word boundary 匹配独立的 `const` 关键字
- `\b` 是单词边界，确保匹配的是完整的单词

**避免匹配**：
- `constexpr` - 不会匹配 ✓
- `const_cast` - 不会匹配 ✓
- `myconst` - 不会匹配 ✓
- `constify` - 不会匹配 ✓

**正确匹配**：
- `const int` - 匹配 ✓
- `int const` - 匹配 ✓
- `int * const` - 匹配 ✓（顶层 const）
- `const int *` - **不匹配**（底层 const，不影响变量本身）

## 预期的测试用例

### 测试用例 1：顶层 const 变量
```rust
#[test]
fn test_is_const_var_top_level() {
    // 顶层 const：变量本身是 const
    assert!(Kind::VarDecl(make_var("const int x")).is_const_var());
    assert!(Kind::VarDecl(make_var("int const x")).is_const_var());
}
```

### 测试用例 2：指针 const 区分
```rust
#[test]
fn test_is_const_var_pointer() {
    // 顶层 const：指针本身是 const
    assert!(Kind::VarDecl(make_var("int * const p")).is_const_var());
    
    // 底层 const：指针指向的对象是 const，不影响变量本身
    assert!(!Kind::VarDecl(make_var("const int * p")).is_const_var());
    assert!(!Kind::VarDecl(make_var("int const * p")).is_const_var());
}
```

### 测试用例 3：不匹配包含 const 子串的关键字
```rust
#[test]
fn test_is_const_var_no_match_keywords() {
    // 不应该匹配包含 const 子串的关键字
    assert!(!Kind::VarDecl(make_var("constexpr int x")).is_const_var());
    assert!(!Kind::VarDecl(make_var("auto const_cast<T>(x)")).is_const_var());
}
```

### 测试用例 4：复杂类型
```rust
#[test]
fn test_is_const_var_complex_types() {
    assert!(Kind::VarDecl(make_var("const struct Point p")).is_const_var());
    assert!(Kind::VarDecl(make_var("struct Point * const p")).is_const_var());
    assert!(!Kind::VarDecl(make_var("const struct Point * p")).is_const_var());
}
```

### 测试用例 5：多级指针
```rust
#[test]
fn test_is_const_var_multi_level_pointer() {
    // 多级指针：只有最后一个 * 后面有 const 才是顶层 const
    assert!(Kind::VarDecl(make_var("int ** const p")).is_const_var());
    assert!(!Kind::VarDecl(make_var("int * const * p")).is_const_var());
}
```

## 实现细节总结

1. **核心逻辑**：
   - 使用 `rfind('*')` 逆向查找最后一个指针
   - 根据是否找到指针决定检查范围
   - 使用 `\bconst\b` 正则匹配独立的 const 关键字

2. **修改点**：
   - `Kind::is_const_var` 方法（第 192-197 行）

3. **优化点**：
   - 提前创建正则表达式，避免重复编译
   - 代码复用，提高效率
   - 逻辑清晰，易读性提高

4. **性能考虑**：
   - 正则编译在每次调用时执行
   - 如果需要进一步优化，可以使用 `lazy_static` 或 `once_cell` 缓存正则
   - 字符串查找是 O(n) 操作，对于复杂类型可接受