# 重构方案：File::remove_static

## 目标概述

将 `remove_static` 函数从 ~55 行拆分为多个职责单一的函数，提高可读性和可测试性。

## 涉及的文件

| 文件 | 变更 |
|------|------|
| `src/file.rs` | 重构 `remove_static`，提取子函数 |

## 当前代码分析

**函数职责**：
1. 计算 md5 哈希
2. 遍历节点，重命名 static 符号为 `_c2rust_private_{md5}_{name}`
3. 生成 C 代码到临时文件
4. 调用 clang 预处理
5. 重试逻辑（fix 标志）
6. 重新加载 AST 并设置 md5

**问题**：
- 函数过长（~55 行）
- 多个职责混在一起
- 文件操作和 clang 调用耦合

## 重构思路

拆分为 3 个函数：

| 函数名 | 职责 | 预估行数 |
|--------|------|----------|
| `rename_static_symbols` | 遍历节点，重命名 static 符号，返回是否有修改 | ~10 |
| `preprocess_c_code` | 生成 C 代码，调用 clang 预处理，支持重试 | ~25 |
| `remove_static` | 主函数，协调调用 | ~15 |

## 伪代码

```rust
impl File {
    fn remove_static(root: &Path, path: &Path, mut node: Node) -> Result<Node> {
        let md5 = Self::md5_file(&path.with_extension("c2rust"))?;
        
        if !Self::rename_static_symbols(&mut node, &md5) {
            return Ok(node);
        }

        let new_c_file = path.with_extension("c");
        Self::preprocess_c_code(root, &node.inner, path, &new_c_file)?;

        let mut new_node = Self::load_by_c_file(root, &new_c_file)?;
        let Kind::TranslationUnitDecl(ref mut unit) = new_node.kind else {
            return Err(Error::inval());
        };
        unit.md5 = Some(Self::md5_file(path)?);
        Ok(new_node)
    }

    fn rename_static_symbols(node: &mut Node, md5: &str) -> bool {
        let mut has_static = false;
        for child in &mut node.inner {
            if !child.kind.is_static() {
                continue;
            }
            let Some(name) = child.kind.name() else {
                continue;
            };
            child.kind.set_global_name(format!("_c2rust_private_{md5}_{name}"));
            has_static = true;
        }
        has_static
    }

    fn preprocess_c_code(root: &Path, nodes: &[Node], path: &Path, output: &Path) -> Result<()> {
        let new_c2rust_file = path.with_extension("c2rust_global");
        let mut fix = false;
        loop {
            let code = Self::generate_c_code(root, nodes, false, fix)?;
            fs::write(&new_c2rust_file, code.as_bytes())
                .log_err(&format!("write {}", new_c2rust_file.display()))?;

            let output_result = Command::new(get_clang())
                .arg("-xc")
                .arg("-E")
                .arg("-C")
                .arg("-P")
                .arg("-fno-builtin")
                .arg(&new_c2rust_file)
                .arg("-o")
                .arg(output)
                .output()
                .log_err("clang -xc -E -P -fno-builtin")?;

            if output_result.status.success() {
                return Ok(());
            }
            if !fix {
                fix = true;
                continue;
            }
            eprintln!("{}", String::from_utf8_lossy(&output_result.stderr));
            return Err(Error::last());
        }
    }
}
```

## 测试用例

### `rename_static_symbols` 单元测试

1. 测试没有 static 符号时返回 false
2. 测试有 static 符号时正确设置 global_name
3. 测试混合 static 和非 static 符号

### `preprocess_c_code` 集成测试

需要 clang 环境：
1. 测试正常预处理流程
2. 测试重试逻辑（fix 标志）
3. 测试 clang 失败时的错误处理
