use hierr::Error;
use std::path::{Path, PathBuf};
pub type Result<T> = core::result::Result<T, Error>;

mod file;
use file::*;
mod feature;
use feature::*;
mod merge;

trait ToError<T, E> {
    fn log_err(self, info: &str) -> Result<T>;
    fn log<F>(self, f: F) -> Result<T>
    where
        F: FnOnce(E);
}

impl<T, E: core::error::Error> ToError<T, E> for core::result::Result<T, E> {
    fn log_err(self, info: &str) -> Result<T> {
        self.map_err(|e| {
            eprintln!("Error--> {} : {info}", e);
            Error::last()
        })
    }
    fn log<F>(self, f: F) -> Result<T>
    where
        F: FnOnce(E),
    {
        self.map_err(|e| {
            f(e);
            Error::last()
        })
    }
}

fn get_root() -> Result<PathBuf> {
    let mut root = Path::new(".").canonicalize().map_err(|_| Error::last())?;
    while !root.join(".c2rust").is_dir() {
        if !root.pop() {
            return Err(Error::noent());
        }
    }
    Ok(root)
}

fn get_clang() -> String {
    std::env::var("C2RUST_CLANG").unwrap_or("clang".to_string())
}

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
    println!(
        "  常规模式：必须指定 --feature 和 exactly one of --init, --update, --reinit, or --merge。"
    );
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

fn main() -> Result<()> {
    // 定义支持的选项: feature: 需要参数, init/update/merge 不需要参数, help/h 显示帮助
    let opts = hiopt::options![
        "feature:",
        "init",
        "update",
        "merge",
        "reinit",
        "build-success",
        "sync",
        "from-feature:",
        "dst-feature:",
        "help",
        "h"
    ];

    // 获取命令行参数（跳过第一个程序名）
    let args: Vec<String> = std::env::args().collect();
    let args_str: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let mut feature_name = None;
    let mut init_flag = false;
    let mut update_flag = false;
    let mut reinit_flag = false;
    let mut merge_flag = false;
    let mut build_success_flag = false;
    let mut sync_flag = false;
    let mut from_feature_name = None;
    let mut dst_feature_name = None;

    // 遍历选项
    for opt in opts.opt_iter(&args_str[..]) {
        match opt {
            Ok((idx, arg)) => {
                let opt_name = opts[idx].name();
                match opt_name {
                    "feature" => {
                        feature_name = arg.map(|s| s.to_string());
                    }
                    "init" => {
                        init_flag = true;
                    }
                    "update" => {
                        update_flag = true;
                    }
                    "reinit" => {
                        reinit_flag = true;
                    }
                    "merge" => {
                        merge_flag = true;
                    }
                    "build-success" => {
                        build_success_flag = true;
                    }
                    "sync" => {
                        sync_flag = true;
                    }
                    "from-feature" => {
                        from_feature_name = arg.map(|s| s.to_string());
                    }
                    "dst-feature" => {
                        dst_feature_name = arg.map(|s| s.to_string());
                    }
                    "help" | "h" => {
                        print_help();
                        return Ok(());
                    }
                    _ => unreachable!(),
                }
            }
            Err(err) => {
                eprintln!("Error parsing options: {:?}", err);
                return Err(Error::inval());
            }
        }
    }

    if sync_flag {
        let src_name = from_feature_name.ok_or_else(|| {
            eprintln!("Error: --from-feature is required when --sync is specified");
            Error::inval()
        })?;
        let dst_name = dst_feature_name.ok_or_else(|| {
            eprintln!("Error: --dst-feature is required when --sync is specified");
            Error::inval()
        })?;

        Feature::sync(&src_name, &dst_name)?;
        return Ok(());
    }

    // 检查是否指定了feature
    let feature_name = feature_name.ok_or_else(|| {
        eprintln!("Error: --feature option is required");
        Error::inval()
    })?;

    // 检查操作标志
    let operations = [
        (init_flag, "init"),
        (update_flag, "update"),
        (reinit_flag, "reinit"),
        (merge_flag, "merge"),
    ];
    let specified: Vec<_> = operations.iter().filter(|(flag, _)| *flag).collect();
    if specified.len() != 1 {
        eprintln!("Error: exactly one of --init, --update, --reinit, or --merge must be specified");
        return Err(Error::inval());
    }

    let mut feature = Feature::new(&feature_name)?;
    match specified[0].1 {
        "init" => {
            feature.init()?;
        }
        "update" => {
            feature.update(build_success_flag)?;
        }
        "reinit" => {
            feature.reinit()?;
        }
        "merge" => {
            feature.merge()?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_update_without_build_success_flag_has_no_copy_side_effects() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let feature_name = "test_feature_no_build_success";
        let rust_src = root.join("rust/src");
        fs::create_dir_all(&rust_src).unwrap();

        let c2rust_dir = root.join(".c2rust");
        fs::create_dir_all(&c2rust_dir).unwrap();

        let feature_dir = root.join(feature_name);
        fs::create_dir_all(&feature_dir).unwrap();
        let rust_dir = feature_dir.join("rust/src/mod_test");
        fs::create_dir_all(&rust_dir).unwrap();

        let private_file = rust_dir.join("fun_c2rust_private_test.rs");
        fs::write(&private_file, "pub fn test_func() {}").unwrap();

        let c_dir = feature_dir.join("c");
        fs::create_dir_all(&c_dir).unwrap();
        let c_file = c_dir.join("test.c");
        fs::write(&c_file, "// C file content").unwrap();

        assert!(private_file.exists());
    }

    #[test]
    fn test_update_with_build_success_flag_copies_private_files_only() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let feature_name = "test_feature_with_build_success";
        let rust_src = root.join("rust/src");
        fs::create_dir_all(&rust_src).unwrap();

        let c2rust_dir = root.join(".c2rust");
        fs::create_dir_all(&c2rust_dir).unwrap();

        let feature_dir = root.join(feature_name);
        fs::create_dir_all(&feature_dir).unwrap();
        let rust_dir = feature_dir.join("rust/src/mod_test");
        fs::create_dir_all(&rust_dir).unwrap();

        let private_file = rust_dir.join("fun_c2rust_private_test.rs");
        fs::write(&private_file, "pub fn test_func() {}").unwrap();

        let rust_dir2 = feature_dir.join("rust/src/mod_test2");
        fs::create_dir_all(&rust_dir2).unwrap();

        let private_file2 = rust_dir2.join("fun_c2rust_private_test.rs");
        fs::write(&private_file2, "").unwrap();

        let c_dir = feature_dir.join("c");
        fs::create_dir_all(&c_dir).unwrap();
        let c_file = c_dir.join("test.c");
        fs::write(&c_file, "// C file content").unwrap();
        let c_file2 = c_dir.join("test2.c");
        fs::write(&c_file2, "// C file content").unwrap();

        assert!(private_file.exists());
        assert!(private_file2.exists());
    }
}
