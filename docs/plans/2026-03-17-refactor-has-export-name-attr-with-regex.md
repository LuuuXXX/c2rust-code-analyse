# 重构 Feature::has_export_name_attr 实现方案

## 目标概述
重构 `Feature::has_export_name_attr` 方法，使用 regex crate 的正则表达式替代手动字符串解析，提高代码简洁性、健壮性和可维护性。

## 涉及文件
- **主要修改**：`src/feature.rs` (第982-1005行，`has_export_name_attr` 函数)
- **依赖**：`regex` 已存在（版本 1.12.2），无需添加

## 技术选型

### 核心思路
1. 使用正则表达式 `r#"export_name\s*=\s*"([^"]*)""#` 匹配 `export_name = "xxx"` 模式
2. 捕获组提取引号内的名字
3. 单次匹配判断，避免重复执行正则引擎

### 关键优化
- 只调用 `re.captures()` 一次，通过 `match` 分发处理逻辑
- 支持任意空白字符格式（`export_name="x"`, `export_name = "x"`, `export_name  =  "x"`）
- 保留原有语义：匹配则保留并标记 `found=true`，不匹配的 `export_name` 属性删除，非 `export_name` 属性保留

### 实现代码
```rust
fn has_export_name_attr(attrs: &mut Vec<syn::Attribute>, expected_name: &str) -> bool {
    let re = Regex::new(r#"export_name\s*=\s*"([^"]*)""#).unwrap();

    let mut found = false;
    attrs.retain(|attr| {
        let s = attr.to_token_stream().to_string();
        match re.captures(&s) {
            Some(caps) => {
                if &caps[1] == expected_name {
                    found = true;
                    true  // 保留匹配的属性
                } else {
                    false // 删除不匹配的 export_name 属性
                }
            }
            None => true  // 没有匹配，保留其他属性
        }
    });
    found
}
```

### 正则表达式解析
- `export_name` - 字面匹配
- `\s*` - 零或多个空白字符
- `=` - 等号
- `\s*` - 等号后的空白字符
- `"` - 开始引号
- `([^"]*)` - 捕获组：提取引号内的名字（非引号字符）
- `"` - 结束引号

## 预期测试用例

### 单元测试场景（添加到 `src/feature.rs` 或 `tests/feature_tests.rs`）

1. **标准格式匹配**：
   - 输入：`export_name = "foo"`，期望：`expected_name="foo"` 返回 true
   - 验证：属性被保留，`found=true`

2. **紧凑格式匹配**：
   - 输入：`export_name="bar"`，期望：`expected_name="bar"` 返回 true
   - 验证：无空格也能正确匹配

3. **多空格格式匹配**：
   - 输入：`export_name  =  "baz"`，期望：`expected_name="baz"` 返回 true
   - 验证：任意空格组合都支持

4. **名字不匹配删除**：
   - 输入：`export_name = "wrong"`，期望：`expected_name="correct"` 返回 false
   - 验证：属性被删除，`found=false`

5. **无 export_name 属性**：
   - 输入：其他属性（如 `#[inline]`），期望：保留原属性，`found=false`
   - 验证：非 `export_name` 属性不受影响

6. **多个属性混合**：
   - 输入：`#[export_name = "target"] #[inline]`，期望：匹配 `target` 时保留两者，`found=true`
   - 验证：混合场景正确处理

## 优势
1. **代码简洁**：从24行减少到约10行
2. **健壮性**：自动处理各种空白格式
3. **可维护性**：正则意图清晰，易于修改
4. **性能**：regex crate 预编译优化，单次匹配

## 注意事项
- 保持原有函数签名不变：`fn has_export_name_attr(attrs: &mut Vec<syn::Attribute>, expected_name: &str) -> bool`
- 保持原有语义：匹配成功时返回 true 并保留属性，不匹配的 `export_name` 属性删除
- 添加必要的 `use regex::Regex;` 导入