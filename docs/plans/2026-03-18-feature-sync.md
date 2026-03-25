# 实现 Feature 同步功能方案

**日期：** 2026-03-18

## 目标概述

为 Feature 结构体添加一个 `sync` 关联方法，用于在两个不同的 feature 之间同步 Rust 代码文件。当满足特定条件时，将源 feature 中的 rs 文件内容拷贝到目标 feature 对应的 rs 文件中。同时在 main.rs 中添加新的命令行参数 `--sync --from-feature <feature> --dst-feature <feature>` 来调用这个功能。

## 涉及的文件和模块

### 主要修改文件：
1. **src/feature.rs** - 添加 `sync` 关联方法
2. **src/main.rs** - 添加命令行参数解析和调用逻辑

### 现有相关代码：
- `get_root()` (main.rs:36-44) - 获取项目根目录
- `Feature::copy_content_to_other_modules()` (feature.rs:811-901) - 同类功能的参考实现
- 命令行参数解析逻辑 (main.rs:76-134)

## 技术选型或修改思路

### 1. Feature::sync 方法设计

**方法签名：**
```rust
impl Feature {
    pub fn sync(src_name: &str, dst_name: &str) -> Result<()> {
        let project_root = get_root()?;
        let src_feature_path = project_root.join(".c2rust").join(src_name);
        let dst_feature_path = project_root.join(".c2rust").join(dst_name);
        let src_rust_src = src_feature_path.join("rust/src");
        let dst_rust_src = dst_feature_path.join("rust/src");

        // 遍历 src_rust_src 下的所有 mod_* 目录
        for entry in WalkDir::new(&src_rust_src)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let src_mod_dir = entry.path();
            if !src_mod_dir.is_dir() || !src_mod_dir.file_name().unwrap_or_default().to_string_lossy().starts_with("mod_") {
                continue;
            }

            // 获取对应的 dst mod 目录
            let mod_name = src_mod_dir.file_name().unwrap();
            let dst_mod_dir = dst_rust_src.join(mod_name);

            // 如果 dst mod 目录不存在，跳过
            if !dst_mod_dir.exists() {
                continue;
            }

            // 遍历 src mod 目录下的 fun_*.rs 和 var_*.rs 文件
            for file_entry in WalkDir::new(&src_mod_dir)
                .min_depth(1)
                .max_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let src_rs_file = file_entry.path();
                if !src_rs_file.is_file() || src_rs_file.extension() != Some(OsStr::new("rs")) {
                    continue;
                }

                let file_name = src_rs_file.file_name().unwrap();
                if !(file_name.to_string_lossy().starts_with("fun_") || file_name.to_string_lossy().starts_with("var_")) {
                    continue;
                }

                let dst_rs_file = dst_mod_dir.join(file_name);

                // 检查 dst rs 文件是否存在且为空
                if !dst_rs_file.exists() {
                    continue;
                }
                let dst_rs_content = fs::read_to_string(&dst_rs_file).log_err(&format!("read {}", dst_rs_file.display()))?;
                if !dst_rs_content.trim().is_empty() {
                    continue;
                }

                // 检查两个 .c 文件是否存在且内容相同
                let src_c_file = src_rs_file.with_extension("c");
                let dst_c_file = dst_rs_file.with_extension("c");

                if !src_c_file.exists() || !dst_c_file.exists() {
                    continue;
                }

                let src_c_content = fs::read_to_string(&src_c_file).log_err(&format!("read {}", src_c_file.display()))?;
                let dst_c_content = fs::read_to_string(&dst_c_file).log_err(&format!("read {}", dst_c_file.display()))?;

                if src_c_content != dst_c_content {
                    continue;
                }

                // 满足所有条件，拷贝 src rs 文件到 dst
                let src_rs_content = fs::read_to_string(&src_rs_file).log_err(&format!("read {}", src_rs_file.display()))?;
                fs::write(&dst_rs_file, src_rs_content.as_bytes())
                    .log_err(&format!("write {}", dst_rs_file.display()))?;

                println!("Synced: {} -> {}", src_rs_file.display(), dst_rs_file.display());
            }
        }

        Ok(())
    }
}
```

**同步条件：**
1. dst_feature 下存在同名 rs 文件
2. dst_feature 下的 rs 文件内容为空
3. 两个 feature 下的同名 .c 文件都存在
4. 两个 .c 文件内容相同
5. src_feature 下的 rs 文件有内容

### 2. 命令行参数设计

**新增参数：**
- `--sync`（布尔标志）- 启用同步功能
- `--from-feature <name>` - 指定源 feature 名称
- `--dst-feature <name>` - 指定目标 feature 名称

**参数解析逻辑：**

在 `hiopt::options!` 中添加：
```rust
let opts = hiopt::options![
    "feature:",
    "init",
    "update",
    "merge",
    "reinit",
    "build-success",
    "sync",           // 新增
    "from-feature:",  // 新增
    "dst-feature:",   // 新增
    "help",
    "h"
];
```

添加布尔变量：
```rust
let mut sync_flag = false;
let mut from_feature_name = None;
let mut dst_feature_name = None;
```

在匹配逻辑中添加处理：
```rust
"sync" => {
    sync_flag = true;
}
"from-feature" => {
    from_feature_name = arg.map(|s| s.to_string());
}
"dst-feature" => {
    dst_feature_name = arg.map(|s| s.to_string());
}
```

**操作检查逻辑：**

修改 operations 数组，添加 sync：
```rust
let operations = [
    (init_flag, "init"),
    (update_flag, "update"),
    (reinit_flag, "reinit"),
    (merge_flag, "merge"),
    (sync_flag, "sync"),  // 新增
];
```

添加参数验证：
```rust
if sync_flag {
    let src_name = from_feature_name.ok_or_else(|| {
        eprintln!("Error: --from-feature is required when --sync is specified");
        Error::inval()
    })?;
    let dst_name = dst_feature_name.ok_or_else(|| {
        eprintln!("Error: --dst-feature is required when --sync is specified");
        Error::inval()
    })?;

    println!("Syncing from '{}' to '{}'...", src_name, dst_name);
    Feature::sync(&src_name, &dst_name)?;
    println!("Sync completed");
    return Ok(());
}
```

**更新帮助信息：**
```rust
fn print_help() {
    println!("用法: code-analyse [选项]");
    println!();
    println!("选项:");
    println!("  --feature <名称>     必需：指定要处理的feature名称");
    println!("  --init               初始化feature，创建新的Rust库项目");
    println!("  --reinit             重新初始化feature, 不影响已经转换的rs文件");
    println!("  --update             更新feature，同步C代码和Rust文件");
    println!("  --build-success      表示代码已编译成功，启用拷贝和时间戳同步");
    println!("  --merge              合并feature，合并分散的Rust文件");
    println!("  --sync               同步两个feature之间的Rust代码");
    println!("  --from-feature <名>  源feature名称（配合--sync使用）");
    println!("  --dst-feature <名>   目标feature名称（配合--sync使用）");
    println!("  -h, --help           显示此帮助信息并退出");
    println!();
    println!("说明:");
    println!("  这是一个C到Rust代码转换工具的一部分，用于管理特定feature的C/Rust代码转换。");
    println!("  常规模式：必须指定 --feature 和 exactly one of --init, --update, --reinit, or --merge。");
    println!("  同步模式：使用 --sync --from-feature <src> --dst-feature <dst>");
    println!();
    println!("示例:");
    println!("  code-analyse --feature my_feature --init");
    println!("  code-analyse --feature my_feature --update");
    println!("  code-analyse --feature my_feature --update --build-success");
    println!("  code-analyse --feature my_feature --reinit");
    println!("  code-analyse --feature my_feature --merge");
    println!("  code-analyse --sync --from-feature src_feat --dst-feature dst_feat");
}
```

## 预期的测试用例

### 测试 1：基本同步功能测试
**目的：** 验证满足所有条件时能正确拷贝文件

**步骤：**
1. 创建两个 feature（src_feature 和 dst_feature）
2. 在 src_feature 中创建 `rust/src/mod_a/fun_foo.rs`（有内容）和对应的 `fun_foo.c`
3. 在 dst_feature 中创建 `rust/src/mod_a/fun_foo.rs`（空文件）和对应的 `fun_foo.c`（内容相同）
4. 调用 `Feature::sync("src_feature", "dst_feature")`
5. 验证 dst_feature 的 `fun_foo.rs` 内容被正确拷贝

**预期结果：**
- dst_feature 的 `fun_foo.rs` 内容与 src_feature 相同
- 控制台输出同步成功的日志

### 测试 2：C 文件内容不同时不拷贝
**目的：** 验证 C 文件内容不同时不会触发拷贝

**步骤：**
1. 创建两个 feature
2. src_feature 中有 `fun_foo.rs`（有内容）和 `fun_foo.c`（内容 A）
3. dst_feature 中有 `fun_foo.rs`（空文件）和 `fun_foo.c`（内容 B，与 A 不同）
4. 调用 `sync` 方法
5. 验证 dst_feature 的 `fun_foo.rs` 仍然为空

**预期结果：**
- dst_feature 的 `fun_foo.rs` 保持为空
- 控制台无同步操作的日志输出

### 测试 3：目标 rs 文件不为空时不拷贝
**目的：** 验证目标 rs 文件非空时不会被覆盖

**步骤：**
1. 创建两个 feature
2. src_feature 中有 `fun_foo.rs`（内容 A）和对应的 `fun_foo.c`（内容相同）
3. dst_feature 中有 `fun_foo.rs`（内容 B，非空）和对应的 `fun_foo.c`（内容相同）
4. 调用 `sync` 方法
5. 验证 dst_feature 的 `fun_foo.rs` 内容保持为 B

**预期结果：**
- dst_feature 的 `fun_foo.rs` 内容不变
- 控制台无同步操作的日志输出

### 测试 4：多个文件批量同步
**目的：** 验证批量处理多个文件的能力

**步骤：**
1. 创建两个 feature
2. src_feature 中有多个模块和文件：
   - `mod_a/fun_foo.rs`（有内容） + `fun_foo.c`
   - `mod_a/var_bar.rs`（有内容） + `var_bar.c`
   - `mod_b/fun_baz.rs`（有内容） + `fun_baz.c`
3. dst_feature 中对应的文件：
   - `mod_a/fun_foo.rs`（空） + `fun_foo.c`（相同）
   - `mod_a/var_bar.rs`（非空） + `var_bar.c`（相同）
   - `mod_b/fun_baz.rs`（空） + `fun_baz.c`（不同）
4. 调用 `sync` 方法
5. 验证只有满足条件的文件被同步

**预期结果：**
- `mod_a/fun_foo.rs` 被同步（rs 空，c 相同）
- `mod_a/var_bar.rs` 未同步（rs 非空）
- `mod_b/fun_baz.rs` 未同步（c 不同）

### 测试 5：命令行参数测试
**目的：** 验证命令行参数正确解析和调用

**步骤：**
1. 准备测试环境
2. 执行：`code-analyse --sync --from-feature src_feat --dst-feature dst_feat`
3. 验证功能正常执行

**测试 6：参数缺失时的错误处理
**目的：** 验证参数验证逻辑

**步骤：**
1. 执行：`code-analyse --sync --from-feature src_feat`（缺少 dst-feature）
2. 验证输出错误信息
3. 执行：`code-analyse --sync --dst-feature dst_feat`（缺少 from-feature）
4. 验证输出错误信息

**预期结果：**
- 输出明确的错误信息
- 程序返回错误状态

## 关键实现细节

### 1. 文件内容比较
使用 `fs::read_to_string` 读取文件内容，直接比较字符串：
```rust
let src_c_content = fs::read_to_string(&src_c_file)?;
let dst_c_content = fs::read_to_string(&dst_c_file)?;
if src_c_content == dst_c_content {
    // 执行拷贝
}
```

### 2. 空文件检查
使用 `content.trim().is_empty()` 判断文件是否为空：
```rust
let content = fs::read_to_string(&file)?;
if content.trim().is_empty() {
    // 文件为空
}
```

### 3. 目录遍历
使用 `WalkDir` 遍历目录：
```rust
for entry in WalkDir::new(&dir)
    .min_depth(1)
    .max_depth(1)
    .into_iter()
    .filter_map(|e| e.ok())
{
    let path = entry.path();
    // 处理路径
}
```

### 4. 错误处理
使用现有的 `Error::log_err` 模式：
```rust
fs::read_to_string(&file).log_err(&format!("read {}", file.display()))?;
```

### 5. 向后兼容
- 保留原有的 `--feature` 参数，但在 `--sync` 模式下不使用
- `--sync` 与 `--init`, `--update`, `--reinit`, `--merge` 互斥
