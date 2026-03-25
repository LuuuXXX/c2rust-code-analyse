# 重构方案：优化文件时间同步算法

## 问题分析

**约束：** c 和 rs 文件的时间戳必须相同

**当前代码逻辑：**
```rust
// needs_validation: 比较 rust_mtime 和 c_mtime
if rust_mtime == c_mtime {
    return Ok(false);  // 时间相同，不需要验证
}

// update_c_file_mtime: 将 c 文件时间设置为 rust 文件时间
filetime::set_file_mtime(&c_file, rust_mtime);
```

**问题：**
- 时间比较只到秒级（`as_secs()`）
- 如果 rs 文件在同一秒内被修改，`needs_validation` 可能返回 `false`
- 导致跳过验证

## 涉及的文件

| 文件 | 变更 |
|------|------|
| `src/feature.rs` | 修改 `update_c_file_mtime` 函数 |

## 重构思路

将两个文件的时间戳都设置为**相同的过去某个固定时间**：
- 使用当前时间减去 1 分钟
- 两个文件使用完全相同的时间戳
- 避免时间精度问题

## 伪代码

```rust
fn update_c_file_mtime(rust_file: &Path) -> Result<()> {
    let c_file = rust_file.with_extension("c");
    
    // 使用过去固定时间（当前时间 - 1 分钟）
    // 避免时间精度问题，同时保证两个文件时间相同
    let mtime = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(60))
        .unwrap_or(std::time::SystemTime::now());
    
    let file_time = filetime::FileTime::from_system_time(mtime);
    
    filetime::set_file_mtime(&c_file, file_time)
        .log_err(&format!("set mtime for {}", c_file.display()))?;
    
    filetime::set_file_mtime(rust_file, file_time)
        .log_err(&format!("set mtime for {}", rust_file.display()))?;
    
    Ok(())
}
```

## 测试

现有测试已覆盖相关功能，无需新增测试用例。
