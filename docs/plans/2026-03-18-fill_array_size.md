# 实现 fill_array_size 函数方案

**日期：** 2026-03-18

## 目标概述

实现 `MyClangType::fill_array_size` 函数，用于将 typedef 类型中的数组维度信息填充到 c_code 的空数组括号中。

### 核心逻辑

1. 用一条正则表达式整体匹配 typedef 尾部的全部数组维度
2. 用一条正则表达式整体匹配 c_code 尾部的全部数组维度
3. 通过统计 `[` 或 `]` 的数量确定维度数
4. 数量必须相等，否则返回错误
5. 直接用 typedef 的整体匹配替换 c_code 的整体匹配

## 涉及的文件和模块

### 主要修改文件：
**src/file.rs** - 实现 `fill_array_size` 方法

### 上下文相关代码：
- `MyClangType` 结构体（file.rs:390-395）
- `typedef()` 方法（file.rs:398-402）
- `ignore_fn()` 方法（file.rs:433-436）
- `ignore_fn_range()` 方法（file.rs:438-461）
- `define_var()` 方法（file.rs:404-414）
- 调用位置（file.rs:865）

## 技术选型或修改思路

### 场景分析

**typedef 尾部可能的格式（从右向左，保证都有维度信息）：**
- `[5]` - 无空格
- `[ 5 ]` - 内部有空格
- `[5] ` - 右侧有空格
- ` [5]` - 左侧有空格
- ` [ 5 ] ` - 前后都有空格
- `[]` - 空括号
- `[] [n]` - 空括号 + 维度
- `[] [n][m]` - 空括号 + 多个维度
- `[n] [m]` - 维度之间有空格
- `[n]  [m]` - 维度之间多个空格

**c_code 尾部可能的格式（通过 define_var 生成）：**
- `[]` - 空括号
- `[ ]` - 内部有空白
- `[ ][]` - 多个空括号
- `[] []` - 多个空括号，有空格
- ` [ ] [ ]` - 多个空括号，带空格
- `[ ]  []` - 多个空括号，多个空格

### 核心正则表达式

```rust
r"(\[\s*\d*\s*\]\s*)+$"
```

**解释：**
- `\[` - 匹配 `[`
- `\s*` - 可选空格（内部）
- `\d*` - 可选数字（支持 `[]` 和 `[5]`）
- `\s*` - 可选空格（内部）
- `\]` - 匹配 `]`
- `\s*` - 可选空格（表达式之间）
- `()` - 捕获整个 `[]` + 后续空格
- `+$` - 重复一次或多次，必须在字符串尾部

**匹配示例：**
- `[5][10]` ✓
- `[5] [10]` ✓
- `[5]  [10]` ✓
- ` [ 5 ] [ 10 ] ` ✓
- `[][]` ✓
- `[] [10]` ✓
- `[5]   [10]   ` ✓

### 实现方案

```rust
fn fill_array_size(&self, c_code: &mut String) -> Result<()> {
    // 1. 获取 typedef 类型，忽略函数指针部分
    let ty = Self::ignore_fn(self.typedef());
    let (off, end) = Self::ignore_fn_range(ty);
    let ty_without_fn = &ty[off..end];
    
    // 2. 一条正则表达式匹配尾部全部的数组维度
    // 允许 [数字]、[]，以及表达式之间有空格
    let array_re = Regex::new(r"(\[\s*\d*\s*\]\s*)+$").unwrap();
    
    // 3. 整体提取 typedef 尾部的数组维度
    let typedef_match = if let Some(cap) = array_re.captures(ty_without_fn) {
        cap[0].to_string()
    } else {
        // typedef 尾部没有数组维度，无需处理
        return Ok(());
    };
    
    // 4. 整体提取 c_code 尾部的数组维度
    let c_code_match = if let Some(cap) = array_re.captures(c_code) {
        cap[0].to_string()
    } else {
        // c_code 尾部没有数组维度，无需处理
        return Ok(());
    };
    
    // 5. 通过统计 [ 的数量确定维度数
    let typedef_count = typedef_match.chars().filter(|&c| c == '[').count();
    let c_code_count = c_code_match.chars().filter(|&c| c == '[').count();
    
    // 6. 验证维度数量是否匹配
    if c_code_count != typedef_count {
        return Err(Error::inval());
    }
    
    // 7. 直接用 typedef 的整体匹配替换 c_code 的整体匹配
    let cap = array_re.captures(c_code).unwrap();
    let (start, end) = (cap.get(0).unwrap().start(), cap.get(0).unwrap().end());
    c_code.replace_range(start..end, &typedef_match);
    
    Ok(())
}
```

### 实现细节说明

**1. 整体提取**

- `array_re.captures(ty_without_fn)` 一次性获取 typedef 尾部的全部数组维度
- `array_re.captures(c_code)` 一次性获取 c_code 尾部的全部数组维度

**2. 统计维度数**

- 通过 `chars().filter(|&c| c == '[').count()` 统计 `[` 的数量
- 或者统计 `]` 的数量（结果相同）

**3. 直接整体替换**

- 使用 `c_code.replace_range(start..end, &typedef_match)` 直接替换
- 不需要逐个处理

## 预期的测试用例

**测试 1：c_code 所有维度都丢失**
```rust
let ty = MyClangType {
    qual_type: "const int name [5] [10]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name [] []".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [5] [10]");
```

**测试 2：c_code 部分维度丢失**
```rust
let ty = MyClangType {
    qual_type: "const int name [5] [10]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name [] [10]".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [5] [10]");
```

**测试 3：typedef 和 c_code 都有空格（表达式之间）**
```rust
let ty = MyClangType {
    qual_type: "const int name [ 5 ] [ 10 ]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name [ ] [ ]".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [ 5 ] [ 10 ]");
```

**测试 4：表达式之间多个空格**
```rust
let ty = MyClangType {
    qual_type: "const int name [5]  [10]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name []  []".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [5]  [10]");
```

**测试 5：非数组类型**
```rust
let ty = MyClangType {
    qual_type: "const int name".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name");
```

**测试 6：c_code 没有括号（不处理）**
```rust
let ty = MyClangType {
    qual_type: "const int name [5]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name");
```

**测试 7：括号数量不匹配（错误）**
```rust
let ty = MyClangType {
    qual_type: "const int name [5] [10]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name []".to_string();
let result = ty.fill_array_size(&mut code);
assert!(result.is_err());
```

**测试 8：typedef 非数组但 c_code 有括号（错误）**
```rust
let ty = MyClangType {
    qual_type: "const int name".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name []".to_string();
let result = ty.fill_array_size(&mut code);
assert!(result.is_err());
```

**测试 9：大量空格场景**
```rust
let ty = MyClangType {
    qual_type: "const int name   [   5   ]   [   10   ]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name   [   ]   [   ]".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name   [   5   ]   [   10   ]");
```

**测试 10：函数指针类型**
```rust
let ty = MyClangType {
    qual_type: "const int (*)(int) [5] [10]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name [] [10]".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [5] [10]");
```

**测试 11：只有空括号**
```rust
let ty = MyClangType {
    qual_type: "const int name []".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name []".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name []");
```

**测试 12：typedef 维度前有空格**
```rust
let ty = MyClangType {
    qual_type: "const int name [5]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name [5]".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [5]");
```

**测试 13：typedef 尾部多个维度，c_code 部分丢失**
```rust
let ty = MyClangType {
    qual_type: "const int name [3] [7] [15]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name [] [7] [15]".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [3] [7] [15]");
```

**测试 14：typedef 和 c_code 维度正确（不改变）**
```rust
let ty = MyClangType {
    qual_type: "const int name [5] [10]".to_string(),
    desugared_qual_type: None,
};
let mut code = "const int name [5] [10]".to_string();
ty.fill_array_size(&mut code).unwrap();
assert_eq!(code, "const int name [5] [10]");
```

## 关键实现细节

### 1. 正则表达式优化

- 使用一条正则表达式 `r"(\[\s*\d*\s*\]\s*)+$"` 整体匹配
- `\s*` 在每个位置处理空格，确保兼容所有空格场景
- `+$` 确保只在字符串尾部匹配

### 2. 维度数量统计

- 通过统计 `[` 字符数量确定维度数
- 简单直接，不需要额外解析

### 3. 整体替换策略

- 直接用 typedef 的匹配字符串替换 c_code 的匹配字符串
- 保留所有空格格式，只替换内容

### 4. 错误处理

- 维度数量不匹配时返回错误
- 使用现有的 `Error::inval()` 错误类型

## 边界情况考虑

1. **空括号场景** - typedef 和 c_code 都可能是 `[]`
2. **所有空格位置** - 内部、外部、表达式之间的空格
3. **部分维度丢失** - c_code 可能只有部分维度
4. **函数指针类型** - 需要忽略函数指针部分
5. **数量不匹配** - 必须返回错误
6. **非数组类型** - 都没有数组维度时直接返回