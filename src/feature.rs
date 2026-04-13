use crate::{get_root, File, Node, Result, ToError};
use hierr::Error;
use quote::{quote, ToTokens};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env::{remove_var, set_var, var};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use syn::{
    parse::Parser,
    spanned::Spanned,
    visit::{visit_file, visit_signature, Visit},
    visit_mut::{visit_file_mut, visit_foreign_item_fn_mut, visit_item_foreign_mod_mut, VisitMut},
};
use toml_edit::{Array, Document, Item, Table};
use walkdir::WalkDir;

pub struct Feature {
    pub root: PathBuf,
    pub prefix: PathBuf,
    pub name: String,
    pub files: Vec<File>,
    fast: bool,
}

impl Feature {
    pub fn new(name: &str) -> Result<Self> {
        let root = get_root()?.join(".c2rust").join(name);
        let prefix = Self::get_file_prefix(&root.join("c"));
        let mut this = Self {
            root,
            name: name.to_string(),
            prefix,
            files: vec![],
            fast: Self::fast_policy(),
        };
        this.get_files()?;
        Ok(this)
    }

    fn fast_policy() -> bool {
        var("C2RUST_CODE_POLICY").as_deref() != Ok("safe")
    }

    fn get_files(&mut self) -> Result<()> {
        let c_root = self.root.join("c");
        // 先收集所有路径并排序，确保处理顺序确定（避免 WalkDir 的平台相关遍历顺序影响结果）
        let mut paths: Vec<PathBuf> = WalkDir::new(&c_root)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.is_file() && p.extension() == Some(OsStr::new("c2rust")))
            .collect();
        paths.sort();
        for path in &paths {
            self.files.push(File::new(&c_root, path)?);
        }
        self.skip_duplicate_weak_fns()?;
        Ok(())
    }

    /// 全局分析同名函数中的弱链接符号，将重复定义的弱链接函数标记为 skip。
    ///
    /// 在 C 代码中，同一个函数可能在多个文件中出现，其中一个（或多个）带有
    /// `__attribute__((weak))` 弱链接属性。Rust 不允许同名函数重复定义，
    /// 因此需要在全局范围内忽略弱链接版本，只保留非弱链接版本。
    /// 若所有同名定义都是弱链接，则保留第一个（按文件路径排序后的顺序），跳过其余的。
    fn skip_duplicate_weak_fns(&mut self) -> Result<()> {
        // 第一遍：统计每个函数名出现的次数，并记录是否存在非弱链接定义
        let mut fn_counts: HashMap<String, usize> = HashMap::new();
        let mut has_non_weak: HashSet<String> = HashSet::new();
        for file in self.files.iter() {
            for node in file.iter() {
                let crate::Kind::FunctionDecl(_) = &node.kind else {
                    continue;
                };
                if node.kind.is_fun_declare(&node.inner) {
                    continue;
                }
                let Some(name) = node.kind.name() else {
                    continue;
                };
                *fn_counts.entry(name.to_string()).or_insert(0) += 1;
                if !node.kind.is_weak_fn(&node.inner) {
                    has_non_weak.insert(name.to_string());
                }
            }
        }

        // 第二遍：精确记录需要跳过的 (file_idx, node_idx) 对。
        // - 若同名函数存在非弱链接定义，则跳过所有弱链接版本。
        // - 若同名函数全部为弱链接（出现多次），则保留第一个，跳过后续的。
        let mut skip_nodes: HashSet<(usize, usize)> = HashSet::new();
        let mut weak_seen: HashSet<String> = HashSet::new();
        for (file_idx, file) in self.files.iter().enumerate() {
            for (node_idx, node) in file.iter().iter().enumerate() {
                let crate::Kind::FunctionDecl(_) = &node.kind else {
                    continue;
                };
                if node.kind.is_fun_declare(&node.inner) {
                    continue;
                }
                let Some(name) = node.kind.name() else {
                    continue;
                };
                let count = fn_counts.get(name).copied().unwrap_or(0);
                if count <= 1 || !node.kind.is_weak_fn(&node.inner) {
                    continue;
                }
                // 当前节点是弱链接且同名函数出现多次
                if has_non_weak.contains(name) {
                    // 存在非弱链接版本 → 跳过所有弱链接版本
                    skip_nodes.insert((file_idx, node_idx));
                } else {
                    // 全部为弱链接 → 保留第一个（按路径排序后的文件顺序），跳过后续的
                    if weak_seen.contains(name) {
                        skip_nodes.insert((file_idx, node_idx));
                    } else {
                        weak_seen.insert(name.to_string());
                    }
                }
            }
        }

        // 第三遍：按文件分组需要 skip 的节点索引，设置标志并保存 JSON
        let mut by_file: HashMap<usize, Vec<usize>> = HashMap::new();
        for (file_idx, node_idx) in skip_nodes {
            by_file.entry(file_idx).or_default().push(node_idx);
        }
        for (file_idx, indices) in by_file {
            let file = &mut self.files[file_idx];
            let nodes = file.iter_mut();
            for idx in &indices {
                nodes[*idx].kind.set_skip();
            }
            file.remove_skipped();
            file.save_json()?;
        }

        Ok(())
    }

    fn get_file_prefix(c_root: &Path) -> PathBuf {
        let mut prefix = c_root.to_path_buf();
        while let Some(child) = Self::get_single_subdir(&prefix) {
            prefix = child;
        }
        prefix
    }

    fn get_single_subdir(path: &Path) -> Option<PathBuf> {
        let mut child = None;
        for entry in WalkDir::new(path)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if child.is_some() || !path.is_dir() {
                return None;
            }
            child = Some(path.to_path_buf());
        }
        child
    }

    // 检查节点是否为需要处理的定义（函数或变量定义）
    fn is_node_definition(node: &crate::Node) -> bool {
        match node.kind {
            crate::Kind::FunctionDecl(_) => !node.kind.is_fun_declare(&node.inner),
            crate::Kind::VarDecl(_) => !node.kind.is_extern() || node.kind.is_inited(),
            _ => false,
        }
    }

    // 根据节点类型生成带前缀的文件名
    fn prefixed_filename(node: &crate::Node) -> Result<String> {
        let name = node.kind.name().ok_or(Error::inval())?;
        let name = Self::normalize_name(name);
        match node.kind {
            crate::Kind::VarDecl(_) => Ok(format!("var_{}", name)),
            crate::Kind::FunctionDecl(_) => Ok(format!("fun_{}", name)),
            _ => Err(Error::inval()),
        }
    }

    pub fn decl_filename(name: &str) -> String {
        let name = Self::normalize_name(name);
        format!("decl_{name}.rs")
    }

    pub fn reinit(&mut self) -> Result<()> {
        println!("Starting reinitialization for feature '{}'", self.name);
        self.create_file_directories()?;
        println!("Feature '{}' reinitialized successfully", self.name);
        set_var("C2RUST_CODE_UPDATE_FORCE", "1");
        self.update(false)?;
        remove_var("C2RUST_CODE_UPDATE_FORCE");
        Ok(())
    }
    ///
    /// 如果全部初始化成功才删除已经存在的内容.
    ///
    pub fn init(&self) -> Result<()> {
        println!("Starting initialization for feature '{}'", self.name);
        let rust = self.root.join("rust");
        let rust_old = self.root.join("rust_old");
        let _ = fs::remove_dir_all(&rust_old);
        let _ = fs::rename(&rust, &rust_old);
        let _ = fs::remove_dir_all(&rust);
        println!("Backed up existing rust directory to rust_old");
        println!("Creating new Rust library project...");
        let output = Command::new("cargo")
            .current_dir(&self.root)
            .arg("new")
            .arg("--lib")
            .arg("--edition")
            .arg("2021")
            .arg("rust")
            .output()
            .log_err("cargo")?;

        if !output.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            return Err(Error::general());
        }
        println!("Rust project created successfully");
        println!("Setting crate type to cdylib...");
        self.set_staticlib()?;
        self.set_lint_rules()?;
        println!("Crate type configured");
        println!("Creating file directory structure...");
        self.create_file_directories()?;
        println!("Directory structure created");
        let _ = fs::remove_dir_all(rust_old);
        println!("Cleaned up backup directory");
        let lib_rs = self.root.join("rust/src/lib.rs");
        let lib_normalized = lib_rs.with_extension("normalized");
        fs::copy(&lib_rs, &lib_normalized).log_err(&format!(
            "copy {} -> {}",
            lib_rs.display(),
            lib_normalized.display()
        ))?;
        println!("Feature '{}' initialized successfully", self.name);
        Ok(())
    }

    /// 检查每个变量和函数翻译状态和对应的rust文件内容是否一致
    /// 如果不一致(已翻译但文件为空或者未翻译但是文件非空), 则根据rust文件内容更新真实的翻译状态,
    /// 更新后需要重新生成C文件.
    pub fn update(&mut self, build_success: bool) -> Result<()> {
        println!("Starting update for feature '{}'", self.name);
        let mut changed = false;
        let prefix = &self.prefix;
        let c_root = self.root.join("c");

        for file in &mut self.files {
            // 获取File对应的mod目录名
            let mod_name = Self::get_mod_name_for_file(prefix, file)?;
            let mod_dir = self.root.join("rust/src").join(&mod_name);

            // 如果mod目录不存在，说明还没有对应的Rust文件，跳过
            if !mod_dir.exists() {
                continue;
            }

            // 收集需要更新的节点及其新状态
            let mut is_updated = false;
            let mut translated = HashMap::new();
            let loaded_from_json = file.loaded_from_json();

            let mod_rs = mod_dir.join("mod.rs");
            let item_names = if mod_rs.exists() {
                Self::collect_item_names_from_mod(&mod_rs)?
            } else {
                HashSet::new()
            };

            for node in file.iter_mut() {
                // 只处理函数和变量定义
                if !Self::is_node_definition(node) || node.kind.is_variadic() {
                    continue;
                }

                let Some(name) = node.kind.name() else {
                    continue;
                };
                let name = name.to_string();

                let prefixed_name = Self::prefixed_filename(node)?;

                let rust_file = mod_dir.join(&prefixed_name).with_extension("rs");
                if !rust_file.exists() {
                    continue;
                }

                let need_validate = Self::needs_validation(&rust_file, loaded_from_json, node.kind.has_committed())?;

                let not_empty = if need_validate {
                    let result = Self::validate_file(
                        &rust_file,
                        &name,
                        &prefixed_name,
                        build_success,
                        &item_names,
                    )?;
                    if build_success {
                        Self::update_c_file_mtime(&rust_file)?;
                    }
                    result
                } else {
                    node.kind.has_committed()
                };

                if not_empty {
                    translated.insert(Self::normalize_name(&name).to_string(), prefixed_name);
                }

                let has_committed = node.kind.has_committed();

                if has_committed != not_empty {
                    node.kind.set_git_commit(not_empty);
                    is_updated = true;
                }
            }

            let mod_rs_needs_sync = Self::mod_rs_needs_sync(&mod_dir, &translated)?;

            // 应用更新到file的节点
            if is_updated || Self::update_force() || mod_rs_needs_sync {
                changed = true;
                Self::update_mod_rs(&mod_dir, translated)?;
                if is_updated || Self::update_force() {
                    // 保存 file 的 JSON 状态，并重导出 C 代码
                    file.save_json()?;
                    let code = file.export_c_code(&c_root)?;
                    let c_file = file.path().with_extension("c");
                    fs::write(&c_file, code.as_bytes())
                        .log_err(&format!("update {}", c_file.display()))?;
                    println!(
                        "Saved JSON state and exported C code for file: {}",
                        mod_name
                    );
                } else {
                    println!("Synchronized module registry for file: {}", mod_name);
                }
            }
        }

        if changed {
            println!("Feature '{}' updated with changes", self.name);
        } else {
            println!("Feature '{}' already up to date", self.name);
        }
        Ok(())
    }

    /// 同步两个 feature 之间的 Rust 代码
    /// 将 src_feature 中有内容的 rs 文件拷贝到 dst_feature 中对应的空 rs 文件
    /// 条件：两个 feature 中对应的 .c 文件内容相同
    pub fn sync(src_name: &str, dst_name: &str) -> Result<()> {
        let project_root = crate::get_root()?;
        let src_feature_path = project_root.join(".c2rust").join(src_name);
        let dst_feature_path = project_root.join(".c2rust").join(dst_name);
        let src_rust_src = src_feature_path.join("rust/src");
        let dst_rust_src = dst_feature_path.join("rust/src");

        println!("Starting sync from '{}' to '{}'...", src_name, dst_name);
        let mut synced_count = 0;

        for entry in WalkDir::new(&src_rust_src)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let src_mod_dir = entry.path();
            if !src_mod_dir.is_dir() {
                continue;
            }

            let mod_name = src_mod_dir
                .file_name()
                .ok_or(Error::inval())?
                .to_string_lossy();
            if !mod_name.starts_with("mod_") {
                continue;
            }

            let dst_mod_dir = dst_rust_src.join(&*mod_name);

            if !dst_mod_dir.exists() {
                continue;
            }

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

                let file_name = src_rs_file
                    .file_name()
                    .ok_or(Error::inval())?
                    .to_string_lossy();
                if !(file_name.starts_with("fun_") || file_name.starts_with("var_")) {
                    continue;
                }

                let dst_rs_file = dst_mod_dir.join(&*file_name);

                if !dst_rs_file.exists() {
                    continue;
                }

                let dst_rs_content = fs::read_to_string(&dst_rs_file)
                    .log_err(&format!("read {}", dst_rs_file.display()))?;
                if !dst_rs_content.trim().is_empty() {
                    continue;
                }

                let src_c_file = src_rs_file.with_extension("c");
                let dst_c_file = dst_rs_file.with_extension("c");

                if !src_c_file.exists() || !dst_c_file.exists() {
                    continue;
                }

                let src_c_content = fs::read_to_string(&src_c_file)
                    .log_err(&format!("read {}", src_c_file.display()))?;
                let dst_c_content = fs::read_to_string(&dst_c_file)
                    .log_err(&format!("read {}", dst_c_file.display()))?;

                if src_c_content != dst_c_content {
                    continue;
                }

                let src_rs_content = fs::read_to_string(&src_rs_file)
                    .log_err(&format!("read {}", src_rs_file.display()))?;
                fs::write(&dst_rs_file, src_rs_content.as_bytes())
                    .log_err(&format!("write {}", dst_rs_file.display()))?;

                println!(
                    "  Synced: {}/{} -> {}/{}",
                    mod_name, file_name, mod_name, file_name
                );
                synced_count += 1;
            }
        }

        println!("Sync completed: {} file(s) synced", synced_count);
        Ok(())
    }

    fn set_staticlib(&self) -> Result<()> {
        let toml_path = self.root.join("rust/Cargo.toml");
        let content =
            fs::read_to_string(&toml_path).log_err(&format!("read {}", toml_path.display()))?;

        let mut doc =
            Document::from_str(&content).log_err(&format!("parse {}", toml_path.display()))?;

        let lib = doc["lib"].or_insert(Item::Table(Table::new()));
        let lib = lib.as_table_mut().ok_or(Error::inval())?;

        let mut crate_type = Array::new();
        crate_type.push("staticlib");
        lib.insert("crate-type", Item::Value(crate_type.into()));

        let toml_string = doc.to_string();
        fs::write(&toml_path, toml_string.as_bytes())
            .log_err(&format!("write {}", toml_path.display()))?;
        Ok(())
    }

    fn set_lint_rules(&self) -> Result<()> {
        let cargo = self.root.join("rust/.cargo");
        fs::create_dir(&cargo).log_err(&format!("create {}", cargo.display()))?;
        let lint = std::env::var_os("C2RUST_HOME").ok_or(Error::inval())?;
        let lint = Path::new(&lint);
        fs::copy(lint.join("conf/lint.toml"), cargo.join("config.toml"))
            .log_err("failed to copy $C2RUST_HOME/conf/lint.toml")?;
        Ok(())
    }

    pub fn lib_attrs() -> &'static str {
r#"// 对应__attribute__((weak))弱链接符号.
// 构建环境需要设置变量RUSTC_BOOTSTRAP=1
#![feature(linkage)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(unsafe_op_in_unsafe_fn)]
// 会生成_Float128浮点数的API，先抑制这类告警.
#![allow(improper_ctypes)]
#![allow(unused_imports)]
#![allow(dead_code)]
"#
    }

    fn add_cstddef_items(items: Vec<syn::Item>) -> Vec<syn::Item> {
        let c_stddef = r"
        use ::core::ffi::*;
        type c_size_t = usize;
        type c_ssize_t = isize;
        type c_ptrdiff_t = isize;
        type c_int8_t = i8;
        type c_int16_t = i16;
        type c_int32_t = i32;
        type c_int64_t = i64;
        type c_uint8_t = u8;
        type c_uint16_t = u16;
        type c_uint32_t = u32;
        type c_uint64_t = u64;
        type uchar = c_uchar;
        type float = c_float;
        type double = c_double;
        type int = c_int;
        type short = c_short;
        type long = c_long;
        type longlong = c_longlong;
        type uint = c_uint;
        type ushort = c_ushort;
        type ulong = c_ulong;
        type ulonglong = c_ulonglong;
        type size_t = c_size_t;
        type ssize_t = c_ssize_t;
        type ptrdiff_t = c_ptrdiff_t;
        type int8_t = c_int8_t;
        type int16_t = c_int16_t;
        type int32_t = c_int32_t;
        type int64_t = c_int64_t;
        type uint8_t = c_uint8_t;
        type uint16_t = c_uint16_t;
        type uint32_t = c_uint32_t;
        type uint64_t = c_uint64_t;
        ";
        let c_items = syn::parse_file(c_stddef).unwrap().items;
        // Collect idents already defined in cstddef to avoid duplicates from bindgen output.
        // Bindgen generates type aliases from C typedefs (e.g. typedef unsigned int uint;),
        // which would conflict with the canonical cstddef definitions.
        let cstddef_names: HashSet<proc_macro2::Ident> = c_items
            .iter()
            .filter_map(|item| {
                if let syn::Item::Type(t) = item {
                    Some(t.ident.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut result = c_items;
        result.extend(items.into_iter().filter(|item| {
            if let syn::Item::Type(t) = item {
                !cstddef_names.contains(&t.ident)
            } else {
                true
            }
        }));
        result
    }

    fn create_file_directories(&self) -> Result<()> {
        let mut code = "// generated by c2rust\n\n".to_string();
        code.push_str(Self::lib_attrs());
        code.push('\n');
        for file in &self.files {
            let mod_name = self.create_file_mod(file)?;
            code.push_str("mod ");
            code.push_str(&mod_name);
            code.push_str(";\n");
        }
        let lib_rs = self.root.join("rust/src/lib.rs");
        fs::write(&lib_rs, code.as_bytes()).log_err(&format!("write {}", lib_rs.display()))?;
        let lib_normalized = self.root.join("rust/src/lib.normalized");
        fs::write(&lib_normalized, code.as_bytes())
            .log_err(&format!("write {}", lib_normalized.display()))?;
        Ok(())
    }

    fn create_file_mod(&self, file: &File) -> Result<String> {
        let mod_name = Self::get_mod_name_for_file(&self.prefix, file)?;
        let mod_dir = self.root.join("rust/src").join(&mod_name);
        fs::create_dir_all(&mod_dir).log_err(&format!("create {}", mod_dir.display()))?;

        let mut nodes = HashMap::new();
        for node in file.iter() {
            if !Self::is_node_definition(node) || node.kind.is_variadic() {
                continue;
            }
            let Some(name) = node.kind.name() else {
                continue;
            };
            // 对于变量，如果有初始化则需要提取其代码否则任何一个都可以.
            nodes
                .entry(name)
                .and_modify(|old| {
                    if node.kind.is_inited() {
                        *old = node;
                    }
                })
                .or_insert(node);
        }

        println!("{mod_name}: Generating type information with bindgen...");
        self.generate_mod_rs(file, &mod_dir, &nodes)?;
        println!("{mod_name}: Type information generated");

        let mut ffi_decl = Self::get_ffi_decl(&mod_dir)?;
        let c_root = self.root.join("c");
        for (name, node) in nodes {
            let normalized_name = Self::normalize_name(name);
            let Some(decl) = ffi_decl.remove(normalized_name) else {
                // Rust中生成的是const常量无需翻译.
                continue;
            };
            // 根据节点类型添加前缀
            let prefixed_name = Self::prefixed_filename(node)?;
            let rs_file = mod_dir.join(&prefixed_name).with_extension("rs");
            if !rs_file.exists() {
                fs::File::create(&rs_file).log_err(&format!("create {}", rs_file.display()))?;
            }
            let c_code = Self::normalize_c_code(&node, &c_root)?;
            let c_file = mod_dir.join(&prefixed_name).with_extension("c");
            fs::write(&c_file, c_code.as_bytes())
                .log_err(&format!("write {}", c_file.display()))?;
            let decl_file = Self::decl_filename(&name);
            let decl_file = mod_dir.join(decl_file);
            let decl = Self::postprocess_decl(&decl);
            let _ =
                fs::write(&decl_file, decl).log_err(&format!("write {}", decl_file.display()))?;
        }
        Ok(mod_name)
    }

    fn append_clang_options(cmd: &mut Command, file: &File) {
        const SHORT_ENUMS: &'static str = "-fshort-enums";

        let opts = file.path().with_extension("c2rust.opts");
        let opts = fs::read_to_string(opts).unwrap_or(String::new());
        if opts.contains(SHORT_ENUMS) {
            cmd.arg(SHORT_ENUMS);
        }
    }

    pub fn generate_mod_rs(
        &self,
        file: &File,
        mod_dir: &Path,
        nodes: &HashMap<&str, &Node>,
    ) -> Result<()> {
        let types_h = mod_dir.join("types.h");
        let header = file.export_header(&self.root.join("c"))?;
        fs::write(&types_h, header.as_bytes()).log_err(&format!("write {}", types_h.display()))?;

        let mut cmd = Command::new("bindgen");
        cmd.current_dir(mod_dir)
            .arg(types_h)
            .arg("-o")
            .arg("mod.rs")
            .arg("--no-layout-tests")
            .arg("--default-enum-style")
            .arg("consts")
            .arg("--no-prepend-enum-name")
            .arg("--disable-nested-struct-naming")
            .arg("--ctypes-prefix")
            .arg("::core::ffi")
            .arg("--")
            .arg("-fno-builtin")
            .arg("-xc")
            .arg("-Wno-duplicate-decl-specifier")
            .arg("-Wno-attributes");

        Self::append_clang_options(&mut cmd, file);

        let output = cmd.output().log_err("bindgen")?;
        if !output.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            return Err(Error::last());
        }
        self.normalize_mod_rs(mod_dir, nodes)
    }

    // 改名以及修改函数入参和返回值
    // *const T -> Option<&'static T>
    // *mut T -> Option<&'static mut T>
    fn normalize_mod_rs(&self, mod_dir: &Path, nodes: &HashMap<&str, &Node>) -> Result<()> {
        let mod_rs = mod_dir.join("mod.rs");
        let content = fs::read_to_string(&mod_rs).log_err(&format!("read {}", mod_rs.display()))?;
        let mut ast = syn::parse_file(&content).log_err(&format!("parse {}", mod_rs.display()))?;
        ast.items = Self::add_cstddef_items(ast.items);
        let use_super =
            syn::parse_str(&format!("#[allow(unused_imports)]\nuse super::*;")).unwrap();
        ast.items.insert(0, use_super);

        struct Visitor<'a>(&'a HashMap<&'a str, &'a Node>, bool);
        // _c2rust_private_仅仅体现在导出符号名中，本模块的代码仍然看到的是C代码中的名字.
        // 本目录下生成的C源码也要删除掉_c2rust_private_前缀
        impl Visitor<'_> {
            fn foreign_item_attrs(name: &str) -> Vec<syn::Attribute> {
                syn::Attribute::parse_outer
                    .parse_str(&format!("#[allow(warnings)]\n#[link_name = \"{}\"]", name))
                    .unwrap()
            }

            fn normalize_item_const(&mut self, item: &mut syn::Item) {
                let syn::Item::Const(c) = item else {
                    return;
                };
                let name = c.ident.to_string();
                let Some(node) = self.0.get(name.as_str()) else {
                    self.visit_item_const_mut(c);
                    return;
                };
                if node.kind.is_const_var() {
                    self.visit_item_const_mut(c);
                    return;
                }
                let ty = &c.ty;
                let ident = &c.ident;
                *item = syn::parse2(quote!(unsafe extern "C" { static mut #ident: #ty; })).unwrap();
                self.visit_item_mut(item);
            }
        }
        // 如果有多个文件，这些文件当前机制下是相互独立的，同一个类型，同一个FFI函数会重复声明
        // 这里抑制这些重复FFI声明可能导致的告警
        impl VisitMut for Visitor<'_> {
            // bindgen可能将C的指针转换为数组引用，引用声明周期需要确定为`static
            fn visit_type_reference_mut(&mut self, refer: &mut syn::TypeReference) {
                refer.lifetime = Some(syn::parse2(quote!('static)).unwrap());
            }

            fn visit_item_mut(&mut self, item: &mut syn::Item) {
                match item {
                    syn::Item::ForeignMod(m) => visit_item_foreign_mod_mut(self, m),
                    syn::Item::Const(_) => self.normalize_item_const(item),
                    syn::Item::Use(item) => item.vis = syn::Visibility::Inherited,
                    _ => {}
                }
            }

            fn visit_item_const_mut(&mut self, item: &mut syn::ItemConst) {
                let name = item.ident.to_string();
                if name.starts_with("_c2rust_private_") {
                    let new_name = name.splitn(5, '_').last().unwrap();
                    item.ident = syn::Ident::new(new_name, item.ident.span());
                }
                self.visit_type_mut(&mut item.ty);
            }

            fn visit_foreign_item_static_mut(&mut self, item: &mut syn::ForeignItemStatic) {
                let name = item.ident.to_string();
                item.attrs = Self::foreign_item_attrs(&name);
                if name.starts_with("_c2rust_private_") {
                    let new_name = name.splitn(5, '_').last().unwrap();
                    item.ident = syn::Ident::new(new_name, item.ident.span());
                }
                self.visit_type_mut(&mut item.ty);
            }

            fn visit_foreign_item_fn_mut(&mut self, item: &mut syn::ForeignItemFn) {
                let name = item.sig.ident.to_string();
                item.attrs = Self::foreign_item_attrs(&name);
                if name.starts_with("_c2rust_private_") {
                    let new_name = name.splitn(5, '_').last().unwrap();
                    item.sig.ident = syn::Ident::new(new_name, item.sig.ident.span());
                }
                if !self.1 && self.0.contains_key(name.as_str()) {
                    visit_foreign_item_fn_mut(self, item);
                }
            }
            fn visit_fn_arg_mut(&mut self, arg: &mut syn::FnArg) {
                let syn::FnArg::Typed(arg) = arg else {
                    return;
                };
                Feature::normalize_type(&mut arg.ty);
            }
            // 返回值不能修改，引用的生命周期未知，如果采用'static，可能导致无法完成翻译.
            /*
            fn visit_return_type_mut(&mut self, retn: &mut syn::ReturnType) {
                let syn::ReturnType::Type(_, ref mut ty) = retn else {
                    return;
                };
                Feature::normalize_type(&mut **ty);
            }
            */
        }
        let mut visitor = Visitor(nodes, self.fast);
        visit_file_mut(&mut visitor, &mut ast);

        // 使用prettyplease格式化输出
        let formatted = prettyplease::unparse(&ast);
        fs::write(&mod_rs, formatted.as_bytes()).log_err(&format!("write {}", mod_rs.display()))?;
        // 保存一个备份，后续更新的时候需要.
        let normalized_rs = mod_rs.with_extension("normalized");
        fs::copy(&mod_rs, &normalized_rs).log_err(&format!(
            "copy {} -> {}",
            mod_rs.display(),
            normalized_rs.display()
        ))?;
        Ok(())
    }

    fn update_mod_rs(mod_dir: &Path, translated: HashMap<String, String>) -> Result<()> {
        let mod_rs = mod_dir.join("mod.rs");
        // 需要从备份文件中读取完整信息
        let normalized_rs = mod_rs.with_extension("normalized");
        let content = fs::read_to_string(&normalized_rs)
            .log_err(&format!("read {}", normalized_rs.display()))?;
        let mut ast =
            syn::parse_file(&content).log_err(&format!("parse {}", normalized_rs.display()))?;
        // translated中只包含FFI内容
        for item in &mut ast.items {
            let syn::Item::ForeignMod(item) = item else {
                continue;
            };
            item.items.retain(|item| {
                if let syn::ForeignItem::Fn(item) = item {
                    return !translated.contains_key(&item.sig.ident.to_string());
                } else if let syn::ForeignItem::Static(item) = item {
                    return !translated.contains_key(&item.ident.to_string());
                }
                true
            });
        }
        let mut modules = Vec::new();
        for module in translated.values() {
            modules.push(syn::parse_str(&format!("mod {module};")).unwrap());
            modules.push(syn::parse_str(&format!("pub use {module}::*;")).unwrap());
        }
        ast.items.extend(modules);

        let formatted = prettyplease::unparse(&ast);
        fs::write(&mod_rs, formatted.as_bytes()).log_err(&format!("write {}", mod_rs.display()))?;
        Ok(())
    }

    fn mod_rs_needs_sync(mod_dir: &Path, translated: &HashMap<String, String>) -> Result<bool> {
        let mod_rs = mod_dir.join("mod.rs");
        if !mod_rs.exists() {
            return Ok(true);
        }

        let content = fs::read_to_string(&mod_rs).log_err(&format!("read {}", mod_rs.display()))?;
        let ast = syn::parse_file(&content).log_err(&format!("parse {}", mod_rs.display()))?;

        let expected: HashSet<String> = translated.values().cloned().collect();
        let mut actual_mods = HashSet::new();
        let mut actual_pub_uses = HashSet::new();

        for item in ast.items {
            match item {
                syn::Item::Mod(item)
                    if item.content.is_none()
                        && (item.ident == "fun"
                            || item.ident == "var"
                            || item.ident.to_string().starts_with("fun_")
                            || item.ident.to_string().starts_with("var_")) =>
                {
                    actual_mods.insert(item.ident.to_string());
                }
                syn::Item::Use(item) if matches!(item.vis, syn::Visibility::Public(_)) => {
                    if let Some(name) = Self::extract_pub_use_module_name(&item.tree) {
                        actual_pub_uses.insert(name);
                    }
                }
                _ => {}
            }
        }

        Ok(actual_mods != expected || actual_pub_uses != expected)
    }

    fn extract_pub_use_module_name(tree: &syn::UseTree) -> Option<String> {
        let syn::UseTree::Path(path) = tree else {
            return None;
        };
        if !matches!(&*path.tree, syn::UseTree::Glob(_)) {
            return None;
        }
        let ident = path.ident.to_string();
        if ident.starts_with("fun_") || ident.starts_with("var_") {
            return Some(ident);
        }
        None
    }

    fn normalize_c_code(node: &Node, c_root: &Path) -> Result<String> {
        let mut code = node.kind.c_code(c_root)?;
        let regex = regex::Regex::new("_c2rust_private_[^_]+_").unwrap();
        let mut off = 0;
        while let Some(m) = regex.find(&code[off..]) {
            off += m.start();
            code.replace_range(off..off + m.len(), "");
        }
        // 如果是全局变量，C可能没有初始值，这种情况下，GLM4.7在目前提示词情况下无法正确翻译.
        // 这里生成的C代码无条件增加0初始值.
        if node.kind.is_fake_inited() {
            code.push_str(" = { 0 }");
        }
        Ok(code)
    }

    fn normalize_name(name: &str) -> &str {
        if name.starts_with("_c2rust_private_") {
            return name.splitn(5, '_').last().unwrap();
        }
        name
    }
    fn normalize_type(ty: &mut syn::Type) {
        let syn::Type::Ptr(ref mut ptr) = ty else {
            return;
        };
        let inner = &mut *ptr.elem;
        // 多级指针只处理最外层
        //Self::normalize_type(inner);
        if ptr.const_token.is_some() {
            *ty = syn::parse2(quote!(Option<& #inner>)).unwrap();
        } else {
            *ty = syn::parse2(quote!(Option<&mut #inner>)).unwrap();
        }
    }

    fn get_ffi_decl(mod_dir: &Path) -> Result<HashMap<String, String>> {
        let normalized_rs = mod_dir.join("mod.normalized");
        let content = fs::read_to_string(&normalized_rs)
            .log_err(&format!("read {}", normalized_rs.display()))?;
        let ast =
            syn::parse_file(&content).log_err(&format!("parse {}", normalized_rs.display()))?;
        struct Visitor<'a>(HashMap<String, String>, &'a str);
        let mut visitor = Visitor(HashMap::new(), &content);
        impl Visit<'_> for Visitor<'_> {
            fn visit_item_foreign_mod(&mut self, m: &syn::ItemForeignMod) {
                for item in &m.items {
                    if let syn::ForeignItem::Fn(ref item) = item {
                        let name = item.sig.ident.to_string();
                        let range = item.sig.span().byte_range();
                        self.0.insert(name, self.1[range].to_string());
                    } else if let syn::ForeignItem::Static(ref item) = item {
                        let name = item.ident.to_string();
                        let range = item.span().byte_range();
                        self.0.insert(name, self.1[range].to_string());
                    }
                }
            }
        }
        visit_file(&mut visitor, &ast);
        Ok(visitor.0)
    }

    // 对decl_文件内容进行后处理，用正则把link_name替换成export_name
    fn postprocess_decl(content: &str) -> String {
        let re = Regex::new(r"\blink_name\b").unwrap();
        re.replace_all(content, "export_name").into_owned()
    }

    // 获取File对应的mod目录名
    pub fn get_mod_name_for_file(prefix: &Path, file: &File) -> Result<String> {
        let rel_path = Self::get_mod_rel_path(prefix, file)?;
        Ok("mod_".to_string()
            + &rel_path
                .display()
                .to_string()
                .replace(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_'), "_"))
    }

    fn get_mod_rel_path(prefix: &Path, file: &File) -> Result<PathBuf> {
        Ok(file
            .path()
            .strip_prefix(prefix)
            .map_err(|_| Error::general())?
            .with_extension(""))
    }

    #[allow(dead_code)]
    // 执行cargo build检查编译
    pub fn cargo_build(&self) -> Result<()> {
        println!("Running cargo build to verify compilation...");
        let output = Command::new("cargo")
            .current_dir(self.root.join("rust"))
            .arg("build")
            .output()
            .log_err("cargo")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Cargo build failed:\n{}", stderr);
            return Err(Error::general());
        }

        println!("Cargo build succeeded!");
        Ok(())
    }

    fn update_force() -> bool {
        matches!(
            var("C2RUST_CODE_UPDATE_FORCE").as_deref(),
            Ok("1") | Ok("true")
        )
    }

    fn needs_validation(rust_file: &Path, loaded_from_json: bool, has_committed: bool) -> Result<bool> {
        // 如果不是从 JSON 文件加载，强制校验
        if !loaded_from_json || Self::update_force() {
            return Ok(true);
        }

        let c_file = rust_file.with_extension("c");

        if !c_file.exists() {
            return Ok(true);
        }

        let rust_meta = fs::metadata(rust_file)
            .log_err(&format!("metadata {}", rust_file.display()))?;
        let rust_mtime = rust_meta
            .modified()
            .log_err("get modified time")?
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .log_err("duration since")?
            .as_secs();

        let c_mtime = fs::metadata(&c_file)
            .log_err(&format!("metadata {}", c_file.display()))?
            .modified()
            .log_err("get modified time")?
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .log_err("duration since")?
            .as_secs();

        // 时间戳相同的情况下也需要保证文件大小和json文件中状态取值的一致性.
        if rust_mtime == c_mtime && (rust_meta.len() > 0) == has_committed {
            return Ok(false);
        }
        Ok(true)
    }

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

    fn is_weak_linkage(file: &Path) -> bool {
        let Ok(content) = fs::read_to_string(file) else {
            return false;
        };
        let content = &content.as_bytes()[..100.min(content.len())];
        let regex = regex::bytes::Regex::new(r"__attribute__\s*\(\s*\(\s*weak\s*\)\s*\)").unwrap();
        regex.is_match(content)
    }

    // 大模型返回的文件可能不符合要求, 或者被意外破坏，需要修正.
    fn validate_file(
        file: &Path,
        link_name: &str,
        prefix: &str,
        build_success: bool,
        item_names: &HashSet<String>,
    ) -> Result<bool> {
        if !file.exists() {
            return Ok(true);
        }
        let content = fs::read_to_string(file).log_err(&format!("read {}", file.display()))?;
        let content = content.trim();
        if content.is_empty() {
            return Ok(false);
        }

        let Ok(mut ast) = syn::parse_file(&content)
            .log(|e| eprintln!("parse {} -> {}", file.display(), e.into_compile_error()))
        else {
            // 大模型生成的代码语法错误，需要大模型修正，直接返回.
            return Ok(true);
        };
        let removed = Self::remove_duplicate_items(&mut ast, item_names)?;

        let is_fun = prefix.starts_with("fun_");
        let name = Self::normalize_name(link_name);
        let decl_file = file.with_file_name(Self::decl_filename(name));
        let is_weak_linkage = Self::is_weak_linkage(&file.with_extension("c"));
        let is_changed = if is_fun {
            Self::validate_fun(&mut ast, name, link_name, is_weak_linkage, &decl_file)
        } else {
            Self::validate_var(&mut ast, name, link_name, is_weak_linkage, &decl_file)
        };

        let Ok(is_changed) = is_changed else {
            eprintln!("*** don't find item {name} in {prefix}");
            // 说明命名错误, 需要重新翻译.
            let _ = fs::write(file, "");
            return Ok(false);
        };

        if is_changed || removed {
            let formatted = prettyplease::unparse(&ast);
            fs::write(file, formatted.as_bytes()).log_err(&format!("write {}", file.display()))?;
        }

        // 新增：检查是否需要拷贝到其他模块
        // 两个条件同时满足才触发：build_success=true 且 link_name 以 _c2rust_private_ 开头
        if build_success && link_name.starts_with("_c2rust_private_") {
            let _ = Self::copy_content_to_other_modules(file);
        }

        Ok(true)
    }

    fn copy_content_to_other_modules(file: &Path) -> Result<()> {
        // 1. 读取当前 Rust 文件内容
        let rust_content = fs::read_to_string(file).log_err(&format!("read {}", file.display()))?;

        // 2. 获取当前 C 文件路径
        let source_c_file = file.with_extension("c");
        if !source_c_file.exists() {
            return Ok(());
        }

        // 3. 读取当前 C 文件内容
        let c_content = fs::read_to_string(&source_c_file)
            .log_err(&format!("read {}", source_c_file.display()))?;

        // 4. 获取当前模块目录（跳过）
        let current_mod_dir = file.parent().ok_or(Error::inval())?;

        // 5. 获取 src 目录
        let src_dir = current_mod_dir.parent().ok_or(Error::inval())?;

        // 6. 获取文件名
        let file_name = file
            .file_name()
            .ok_or(Error::inval())?
            .to_string_lossy()
            .to_string();

        // 7. 使用 WalkDir 遍历 src 目录下一层目录
        for entry in WalkDir::new(&src_dir)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let mod_dir = entry.path();

            // 跳过非目录或非 mod_ 开头的目录
            if !mod_dir.is_dir() {
                continue;
            }
            let mod_name = mod_dir
                .file_name()
                .ok_or(Error::inval())?
                .to_string_lossy()
                .to_string();
            if !mod_name.starts_with("mod_") {
                continue;
            }

            // 跳过当前模块
            if mod_dir == current_mod_dir {
                continue;
            }

            // 查找同名文件
            let target_rust_file = mod_dir.join(&file_name);
            if !target_rust_file.exists() {
                continue;
            }

            // 检查目标文件内容是否为空
            let target_rust_content = fs::read_to_string(&target_rust_file)
                .log_err(&format!("read {}", target_rust_file.display()))?;
            if !target_rust_content.trim().is_empty() {
                continue;
            }

            // 检查 C 文件是否存在且内容相同
            let target_c_file = target_rust_file.with_extension("c");
            if !target_c_file.exists() {
                continue;
            }
            let target_c_content = fs::read_to_string(&target_c_file)
                .log_err(&format!("read {}", target_c_file.display()))?;
            if c_content != target_c_content {
                continue;
            }

            // 拷贝内容
            fs::write(&target_rust_file, rust_content.as_bytes())
                .log_err(&format!("write {}", target_rust_file.display()))?;

            println!(
                "Copied content from {} to {}",
                file.display(),
                target_rust_file.display()
            );
        }

        Ok(())
    }

    fn has_ptr_arg(sig: &syn::Signature) -> bool {
        struct Visitor(bool);
        impl Visit<'_> for Visitor {
            fn visit_return_type(&mut self, _: &syn::ReturnType) {}
            fn visit_type_ptr(&mut self, _: &syn::TypePtr) {
                self.0 = true;
            }
        }

        let mut visitor = Visitor(false);
        visit_signature(&mut visitor, sig);
        visitor.0
    }

    fn validate_fun(
        ast: &mut syn::File,
        name: &str,
        link_name: &str,
        is_weak_linkage: bool,
        decl_file: &Path,
    ) -> Result<bool> {
        let mut main_item = None;
        ast.items.iter_mut().any(|item| match item {
            syn::Item::Fn(item) if item.sig.ident.to_string() == name => {
                main_item = Some(item);
                true
            }
            _ => false,
        });
        let Some(main_item) = main_item else {
            return Err(Error::inval());
        };
        let is_unsafe = Self::has_ptr_arg(&main_item.sig);
        // 确保abi等满足要求.
        let mut is_changed = Self::remove_no_mangle_attr(&mut main_item.attrs);
        is_changed |= Self::update_fn_signature(main_item, decl_file)?;

        if main_item.sig.unsafety.is_some() ^ is_unsafe {
            is_changed = true;
            if is_unsafe {
                main_item.sig.unsafety = syn::parse_str("unsafe").unwrap();
            } else {
                main_item.sig.unsafety = None;
            }
        }
        if main_item.sig.abi.is_none() {
            is_changed = true;
            main_item.sig.abi = syn::parse_str(r#"extern "C""#).unwrap();
        }
        if !Self::has_export_name_attr(&mut main_item.attrs, link_name) {
            is_changed = true;
            main_item.attrs.extend(
                syn::Attribute::parse_outer
                    .parse_str(&format!(r#"#[unsafe(export_name = "{link_name}")]"#))
                    .unwrap(),
            );
        }
        let expected_vis: syn::Visibility = if link_name != name {
            syn::parse_str("pub(super)").unwrap()
        } else {
            syn::parse_str("pub").unwrap()
        };
        if !Self::syn_equal(&main_item.vis, &expected_vis) {
            is_changed = true;
            main_item.vis = expected_vis;
        }
        let updated = Self::update_weak_linkage(&mut main_item.attrs, is_weak_linkage);
        is_changed |= updated;
        Ok(is_changed)
    }

    fn syn_equal<ARG: ToTokens>(arg1: &ARG, arg2: &ARG) -> bool {
        arg1.to_token_stream().to_string() == arg2.to_token_stream().to_string()
    }

    fn update_fn_signature(fn_item: &mut syn::ItemFn, decl_file: &Path) -> Result<bool> {
        let content =
            fs::read_to_string(decl_file).log_err(&format!("read {}", decl_file.display()))?;
        let mut sig: syn::Signature = syn::parse_str(&content)
            .log_err(&format!("parse {} -> {content}", decl_file.display()))?;
        let mut is_changed = false;
        fn_item
            .sig
            .inputs
            .iter_mut()
            .zip(sig.inputs.iter_mut())
            .for_each(|args| {
                if let (syn::FnArg::Typed(ref mut arg), syn::FnArg::Typed(ref mut decl)) = args {
                    if !Self::syn_equal(arg, decl) {
                        is_changed = true;
                        core::mem::swap(&mut arg.ty, &mut decl.ty);
                    }
                }
            });
        if !Self::syn_equal(&fn_item.sig.output, &sig.output) {
            is_changed = true;
            core::mem::swap(&mut fn_item.sig.output, &mut sig.output);
        }
        Ok(is_changed)
    }

    fn update_static_type(static_item: &mut syn::ItemStatic, decl_file: &Path) -> Result<bool> {
        let content =
            fs::read_to_string(decl_file).log_err(&format!("read {}", decl_file.display()))?;
        let mut item: syn::ForeignItemStatic = syn::parse_str(&content)
            .log_err(&format!("parse {} -> {content}", decl_file.display()))?;

        if Self::should_preserve_static_array_type(static_item, &item.ty) {
            return Ok(false);
        }

        if !Self::syn_equal(&item.ty, &static_item.ty) {
            core::mem::swap(&mut item.ty, &mut static_item.ty);
            return Ok(true);
        }
        Ok(false)
    }

    fn should_preserve_static_array_type(
        static_item: &syn::ItemStatic,
        decl_ty: &syn::Type,
    ) -> bool {
        let syn::Type::Array(current_array) = &*static_item.ty else {
            return false;
        };
        let syn::Type::Array(decl_array) = decl_ty else {
            return false;
        };

        let Some(init_len) = Self::extract_expr_array_len(&static_item.expr) else {
            return false;
        };
        let Some(current_len) = Self::extract_const_usize(&current_array.len) else {
            return false;
        };
        let Some(decl_len) = Self::extract_const_usize(&decl_array.len) else {
            return false;
        };

        current_len == init_len && decl_len != init_len
    }

    fn extract_expr_array_len(expr: &syn::Expr) -> Option<usize> {
        match expr {
            syn::Expr::Repeat(repeat) => Self::extract_const_usize(&repeat.len),
            syn::Expr::Array(array) => Some(array.elems.len()),
            _ => None,
        }
    }

    fn extract_const_usize(expr: &syn::Expr) -> Option<usize> {
        let syn::Expr::Lit(expr_lit) = expr else {
            return None;
        };
        let syn::Lit::Int(lit_int) = &expr_lit.lit else {
            return None;
        };
        lit_int.base10_parse::<usize>().ok()
    }

    fn validate_var(
        ast: &mut syn::File,
        name: &str,
        link_name: &str,
        is_weak_linkage: bool,
        decl_file: &Path,
    ) -> Result<bool> {
        let mut main_item = None;
        ast.items.iter_mut().any(|item| match item {
            syn::Item::Static(item) if item.ident.to_string() == name => {
                main_item = Some(item);
                true
            }
            _ => false,
        });
        let Some(main_item) = main_item else {
            return Err(Error::inval());
        };
        let mut is_changed = Self::remove_no_mangle_attr(&mut main_item.attrs);
        is_changed |= Self::update_static_type(main_item, decl_file)?;

        if !Self::has_export_name_attr(&mut main_item.attrs, link_name) {
            is_changed = true;
            main_item.attrs.extend(
                syn::Attribute::parse_outer
                    .parse_str(&format!(r#"#[unsafe(export_name = "{link_name}")]"#))
                    .unwrap(),
            );
        }

        if matches!(main_item.mutability, syn::StaticMutability::None) {
            is_changed = true;
            main_item.mutability = syn::parse_str("mut").unwrap();
        }

        let expected_vis: syn::Visibility = if link_name != name {
            syn::parse_str("pub(super)").unwrap()
        } else {
            syn::parse_str("pub").unwrap()
        };
        if !Self::syn_equal(&main_item.vis, &expected_vis) {
            is_changed = true;
            main_item.vis = expected_vis;
        }
        let updated = Self::update_weak_linkage(&mut main_item.attrs, is_weak_linkage);
        is_changed |= updated;
        Ok(is_changed)
    }

    // 返回是否修改
    fn update_weak_linkage(attrs: &mut Vec<syn::Attribute>, is_weak_linkage: bool) -> bool {
        let re = Regex::new(r#"linkage\s*=\s*"weak""#).unwrap();

        let mut weak = false;
        let mut is_changed = false;
        attrs.retain(|attr| {
            let s = attr.to_token_stream().to_string();
            if re.is_match(&s) {
                weak = true;
                is_changed = !is_weak_linkage;
                return is_weak_linkage;
            }
            true
        });

        if is_weak_linkage && !weak {
            is_changed = true;
            attrs.extend(
                syn::Attribute::parse_outer
                    .parse_str(&format!(r#"#[linkage = "weak"]"#))
                    .unwrap(),
            );
        }

        is_changed
    }

    fn has_export_name_attr(attrs: &mut Vec<syn::Attribute>, expected_name: &str) -> bool {
        let re = Regex::new(r#"export_name\s*=\s*"([^"]*)""#).unwrap();

        let mut found = false;
        attrs.retain(|attr| {
            let s = attr.to_token_stream().to_string();
            match re.captures(&s) {
                Some(caps) => {
                    if &caps[1] == expected_name {
                        found = true;
                        true
                    } else {
                        false
                    }
                }
                None => true,
            }
        });
        found
    }
    fn remove_no_mangle_attr(attrs: &mut Vec<syn::Attribute>) -> bool {
        let len = attrs.len();
        attrs.retain(|attr| {
            let s = attr.to_token_stream().to_string();
            !s.contains("no_mangle") && !s.contains("link_name")
        });
        len != attrs.len()
    }

    fn collect_item_names_from_mod(mod_rs: &Path) -> Result<HashSet<String>> {
        // 已经翻译的函数不在mod.rs中，只有从mod.normalized中读取才能保证无遗漏.
        let mod_rs = mod_rs.with_extension("normalized");
        let content = fs::read_to_string(&mod_rs).log_err(&format!("read {}", mod_rs.display()))?;
        let mod_ast = syn::parse_file(&content).log_err(&format!("parse {}", mod_rs.display()))?;

        struct Visitor(HashSet<String>);
        impl Visit<'_> for Visitor {
            fn visit_foreign_item_fn(&mut self, f: &syn::ForeignItemFn) {
                self.0.insert(f.sig.ident.to_string());
            }
            fn visit_foreign_item_static(&mut self, v: &syn::ForeignItemStatic) {
                self.0.insert(v.ident.to_string());
            }
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

        let mut visitor = Visitor(HashSet::new());
        visit_file(&mut visitor, &mod_ast);
        Ok(visitor.0)
    }

    fn remove_duplicate_items(ast: &mut syn::File, item_names: &HashSet<String>) -> Result<bool> {
        struct Visitor<'a>(&'a HashSet<String>, bool);
        impl VisitMut for Visitor<'_> {
            // 翻译的时候可能会定义一些新的类型，改变可见性，不要扩散到其他函数.
            fn visit_visibility_mut(&mut self, v: &mut syn::Visibility) {
                *v = syn::Visibility::Inherited;
            }
            fn visit_item_foreign_mod_mut(&mut self, m: &mut syn::ItemForeignMod) {
                let len = m.items.len();
                m.items.retain(|item| match item {
                    syn::ForeignItem::Fn(f) => !self.0.contains(&f.sig.ident.to_string()),
                    syn::ForeignItem::Static(v) => !self.0.contains(&v.ident.to_string()),
                    _ => true,
                });
                if len != m.items.len() {
                    self.1 = true;
                }
            }

            fn visit_item_mut(&mut self, i: &mut syn::Item) {
                syn::visit_mut::visit_item_mut(self, i);
                let should_remove = match i {
                    syn::Item::Struct(s) => self.0.contains(&s.ident.to_string()),
                    syn::Item::Union(u) => self.0.contains(&u.ident.to_string()),
                    syn::Item::Type(t) => self.0.contains(&t.ident.to_string()),
                    syn::Item::Const(c) => self.0.contains(&c.ident.to_string()),
                    syn::Item::Verbatim(_) => true,
                    _ => false,
                };
                if should_remove {
                    self.1 = true;
                    *i = syn::Item::Verbatim(quote::quote!());
                }
            }
        }
        let mut visitor = Visitor(item_names, false);
        visit_file_mut(&mut visitor, ast);

        // Verbatim一定是非法语句，遇到过大模型返回这类语句，强制删除，否则prettyplease::unparse会panic.
        ast.items.retain(|item| {
            !matches!(item, syn::Item::Verbatim(_))
                && !matches!(item, syn::Item::ForeignMod(m) if m.items.is_empty())
        });

        Ok(visitor.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write `content` to both `mod.rs` and `mod.normalized` in `mod_dir`,
    /// matching the production layout that `collect_item_names_from_mod` expects.
    fn write_mod_files(mod_dir: &Path, content: &str) {
        fs::write(mod_dir.join("mod.rs"), content).unwrap();
        fs::write(mod_dir.join("mod.normalized"), content).unwrap();
    }

    fn create_test_rust_structure(temp_dir: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let mod_a = temp_dir.join("rust/src/mod_a");
        let mod_b = temp_dir.join("rust/src/mod_b");
        fs::create_dir_all(&mod_a).unwrap();
        fs::create_dir_all(&mod_b).unwrap();

        let fun_foo_a_rs = mod_a.join("fun_foo.rs");
        let fun_foo_b_rs = mod_b.join("fun_foo.rs");
        let fun_foo_a_c = mod_a.join("fun_foo.c");
        let fun_foo_b_c = mod_b.join("fun_foo.c");

        (fun_foo_a_rs, fun_foo_b_rs, fun_foo_a_c, fun_foo_b_c)
    }

    #[test]
    fn test_copy_content_to_other_modules_basic() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let (fun_foo_a_rs, fun_foo_b_rs, fun_foo_a_c, fun_foo_b_c) =
            create_test_rust_structure(root);

        fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&fun_foo_b_rs, "").unwrap();
        fs::write(&fun_foo_a_c, "int foo() { return 42; }").unwrap();
        fs::write(&fun_foo_b_c, "int foo() { return 42; }").unwrap();

        Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();

        let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
        assert_eq!(content_b.trim(), "pub fn foo() -> i32 { 42 }");
    }

    #[test]
    fn test_copy_content_c_content_different() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let (fun_foo_a_rs, fun_foo_b_rs, fun_foo_a_c, fun_foo_b_c) =
            create_test_rust_structure(root);

        fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&fun_foo_b_rs, "").unwrap();
        fs::write(&fun_foo_a_c, "int foo() { return 42; }").unwrap();
        fs::write(&fun_foo_b_c, "int foo() { return 100; }").unwrap();

        Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();

        let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
        assert!(content_b.trim().is_empty());
    }

    #[test]
    fn test_copy_content_target_not_empty() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let (fun_foo_a_rs, fun_foo_b_rs, fun_foo_a_c, fun_foo_b_c) =
            create_test_rust_structure(root);

        fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&fun_foo_b_rs, "pub fn bar() -> i32 { 100 }").unwrap();
        fs::write(&fun_foo_a_c, "int foo() { return 42; }").unwrap();
        fs::write(&fun_foo_b_c, "int foo() { return 42; }").unwrap();

        Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();

        let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
        assert_eq!(content_b.trim(), "pub fn bar() -> i32 { 100 }");
    }

    #[test]
    fn test_copy_content_source_c_not_exist() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let (fun_foo_a_rs, fun_foo_b_rs, _, _) = create_test_rust_structure(root);

        fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&fun_foo_b_rs, "").unwrap();

        Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();

        let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
        assert!(content_b.trim().is_empty());
    }

    #[test]
    fn test_copy_content_multiple_targets() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let mod_a = root.join("rust/src/mod_a");
        let mod_b = root.join("rust/src/mod_b");
        let mod_c = root.join("rust/src/mod_c");
        fs::create_dir_all(&mod_a).unwrap();
        fs::create_dir_all(&mod_b).unwrap();
        fs::create_dir_all(&mod_c).unwrap();

        let fun_foo_a_rs = mod_a.join("fun_foo.rs");
        let fun_foo_b_rs = mod_b.join("fun_foo.rs");
        let fun_foo_c_rs = mod_c.join("fun_foo.rs");
        let fun_foo_a_c = mod_a.join("fun_foo.c");
        let fun_foo_b_c = mod_b.join("fun_foo.c");
        let fun_foo_c_c = mod_c.join("fun_foo.c");

        fs::write(&fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&fun_foo_b_rs, "").unwrap();
        fs::write(&fun_foo_c_rs, "").unwrap();
        fs::write(&fun_foo_a_c, "int foo() { return 42; }").unwrap();
        fs::write(&fun_foo_b_c, "int foo() { return 42; }").unwrap();
        fs::write(&fun_foo_c_c, "int foo() { return 42; }").unwrap();

        Feature::copy_content_to_other_modules(&fun_foo_a_rs).unwrap();

        let content_b = fs::read_to_string(&fun_foo_b_rs).unwrap();
        let content_c = fs::read_to_string(&fun_foo_c_rs).unwrap();
        assert_eq!(content_b.trim(), "pub fn foo() -> i32 { 42 }");
        assert_eq!(content_c.trim(), "pub fn foo() -> i32 { 42 }");
    }

    #[test]
    fn test_remove_duplicate_items_struct() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let mod_dir = root.join("rust/src/mod_a");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        let target_rs = mod_dir.join("target.rs");

        let mod_content = r#"
struct MyStruct {
    x: i32,
}
"#;
        let target_content = r#"
struct MyStruct {
    x: i32,
}

struct OtherStruct {
    y: f64,
}
"#;

        write_mod_files(&mod_dir, mod_content);
        fs::write(&target_rs, target_content).unwrap();

        let mut ast = syn::parse_file(&fs::read_to_string(&target_rs).unwrap()).unwrap();
        let item_names = Feature::collect_item_names_from_mod(&mod_rs).unwrap();
        Feature::remove_duplicate_items(&mut ast, &item_names).unwrap();

        let result = prettyplease::unparse(&ast);
        assert!(!result.contains("struct MyStruct"));
        assert!(result.contains("struct OtherStruct"));
    }

    #[test]
    fn test_remove_duplicate_items_type() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let mod_dir = root.join("rust/src/mod_a");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        let target_rs = mod_dir.join("target.rs");

        let mod_content = r#"
type MyType = i32;
"#;
        let target_content = r#"
type MyType = i32;

type OtherType = f64;
"#;

        write_mod_files(&mod_dir, mod_content);
        fs::write(&target_rs, target_content).unwrap();

        let mut ast = syn::parse_file(&fs::read_to_string(&target_rs).unwrap()).unwrap();
        let item_names = Feature::collect_item_names_from_mod(&mod_rs).unwrap();
        Feature::remove_duplicate_items(&mut ast, &item_names).unwrap();

        let result = prettyplease::unparse(&ast);
        assert!(!result.contains("type MyType"));
        assert!(result.contains("type OtherType"));
    }

    #[test]
    fn test_remove_duplicate_items_mixed() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let mod_dir = root.join("rust/src/mod_a");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        let target_rs = mod_dir.join("target.rs");

        let mod_content = r#"
struct MyStruct {
    x: i32,
}

union MyUnion {
    x: i32,
    y: f64,
}

type MyType = i32;

const MY_CONST: i32 = 42;
"#;
        let target_content = r#"
struct MyStruct {
    x: i32,
}

union MyUnion {
    x: i32,
    y: f64,
}

type MyType = i32;

const MY_CONST: i32 = 42;

static MY_STATIC: i32 = 100;

struct KeepStruct {
    z: bool,
}
"#;

        write_mod_files(&mod_dir, mod_content);
        fs::write(&target_rs, target_content).unwrap();

        let mut ast = syn::parse_file(&fs::read_to_string(&target_rs).unwrap()).unwrap();
        let item_names = Feature::collect_item_names_from_mod(&mod_rs).unwrap();
        Feature::remove_duplicate_items(&mut ast, &item_names).unwrap();

        let result = prettyplease::unparse(&ast);
        assert!(!result.contains("struct MyStruct"));
        assert!(!result.contains("union MyUnion"));
        assert!(!result.contains("type MyType"));
        assert!(!result.contains("const MY_CONST"));
        assert!(result.contains("static MY_STATIC"));
        assert!(result.contains("struct KeepStruct"));
    }

    #[test]
    fn test_add_cstddef_items_deduplicates_bindgen_types() {
        // Simulate bindgen output that defines types already present in the cstddef block.
        // This happens when C headers contain typedefs like `typedef unsigned int uint;`.
        let bindgen_output = r#"
pub type uint = ::core::ffi::c_uint;
pub type ushort = ::core::ffi::c_ushort;
pub type ulong = ::core::ffi::c_ulong;
pub struct MyStruct {
    pub x: u32,
}
"#;
        let items = syn::parse_file(bindgen_output).unwrap().items;
        let result = Feature::add_cstddef_items(items);

        let type_names: Vec<String> = result
            .iter()
            .filter_map(|item| {
                if let syn::Item::Type(t) = item {
                    Some(t.ident.to_string())
                } else {
                    None
                }
            })
            .collect();

        // uint, ushort, ulong should appear exactly once (from the cstddef block)
        assert_eq!(type_names.iter().filter(|n| n.as_str() == "uint").count(), 1);
        assert_eq!(
            type_names.iter().filter(|n| n.as_str() == "ushort").count(),
            1
        );
        assert_eq!(
            type_names.iter().filter(|n| n.as_str() == "ulong").count(),
            1
        );
        // MyStruct should still be present
        let has_my_struct = result.iter().any(|item| {
            if let syn::Item::Struct(s) = item {
                s.ident.to_string() == "MyStruct"
            } else {
                false
            }
        });
        assert!(has_my_struct);
    }

    #[test]
    fn test_has_export_name_attr_standard_format() {
        let mut attrs: Vec<syn::Attribute> = vec![syn::parse_quote! { #[export_name = "foo"] }];
        let result = Feature::has_export_name_attr(&mut attrs, "foo");
        assert!(result);
        assert_eq!(attrs.len(), 1);
    }

    #[test]
    fn test_has_export_name_attr_compact_format() {
        let mut attrs: Vec<syn::Attribute> = vec![syn::parse_quote! { #[export_name="bar"] }];
        let result = Feature::has_export_name_attr(&mut attrs, "bar");
        assert!(result);
        assert_eq!(attrs.len(), 1);
    }

    #[test]
    fn test_has_export_name_attr_multiple_spaces() {
        let mut attrs: Vec<syn::Attribute> = vec![syn::parse_quote! { #[export_name  =  "baz"] }];
        let result = Feature::has_export_name_attr(&mut attrs, "baz");
        assert!(result);
        assert_eq!(attrs.len(), 1);
    }

    #[test]
    fn test_has_export_name_attr_wrong_name() {
        let mut attrs: Vec<syn::Attribute> = vec![syn::parse_quote! { #[export_name = "wrong"] }];
        let result = Feature::has_export_name_attr(&mut attrs, "correct");
        assert!(!result);
        assert_eq!(attrs.len(), 0);
    }

    #[test]
    fn test_has_export_name_attr_no_export_name() {
        let mut attrs: Vec<syn::Attribute> = vec![syn::parse_quote! { #[inline] }];
        let result = Feature::has_export_name_attr(&mut attrs, "foo");
        assert!(!result);
        assert_eq!(attrs.len(), 1);
    }

    #[test]
    fn test_has_export_name_attr_multiple_attrs() {
        let mut attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote! { #[export_name = "target"] },
            syn::parse_quote! { #[inline] },
        ];
        let result = Feature::has_export_name_attr(&mut attrs, "target");
        assert!(result);
        assert_eq!(attrs.len(), 2);
    }

    #[test]
    fn test_has_export_name_attr_mixed_export_names() {
        let mut attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote! { #[export_name = "keep"] },
            syn::parse_quote! { #[export_name = "delete"] },
        ];
        let result = Feature::has_export_name_attr(&mut attrs, "keep");
        assert!(result);
        assert_eq!(attrs.len(), 1);
        assert!(attrs[0].to_token_stream().to_string().contains("keep"));
    }

    fn create_sync_test_structure(
        temp_dir: &Path,
        src_name: &str,
        dst_name: &str,
    ) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let c2rust_dir = temp_dir.join(".c2rust");
        fs::create_dir_all(&c2rust_dir).unwrap();

        let src_feature = c2rust_dir.join(src_name);
        let dst_feature = c2rust_dir.join(dst_name);

        let src_mod = src_feature.join("rust/src/mod_a");
        let dst_mod = dst_feature.join("rust/src/mod_a");
        fs::create_dir_all(&src_mod).unwrap();
        fs::create_dir_all(&dst_mod).unwrap();

        let src_fun_rs = src_mod.join("fun_foo.rs");
        let dst_fun_rs = dst_mod.join("fun_foo.rs");
        let src_fun_c = src_mod.join("fun_foo.c");
        let dst_fun_c = dst_mod.join("fun_foo.c");

        (src_fun_rs, dst_fun_rs, src_fun_c, dst_fun_c)
    }

    #[test]
    fn test_sync_basic() {
        let temp_dir = TempDir::new().unwrap();
        let test_root = temp_dir.path();

        let (src_fun_rs, dst_fun_rs, src_fun_c, dst_fun_c) =
            create_sync_test_structure(test_root, "src_feat", "dst_feat");

        fs::write(&src_fun_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&dst_fun_rs, "").unwrap();
        fs::write(&src_fun_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_c, "int foo() { return 42; }").unwrap();

        std::env::set_current_dir(test_root).unwrap();
        Feature::sync("src_feat", "dst_feat").unwrap();

        let content = fs::read_to_string(&dst_fun_rs).unwrap();
        assert_eq!(content.trim(), "pub fn foo() -> i32 { 42 }");
    }

    #[test]
    fn test_sync_c_content_different() {
        let temp_dir = TempDir::new().unwrap();
        let test_root = temp_dir.path();

        let (src_fun_rs, dst_fun_rs, src_fun_c, dst_fun_c) =
            create_sync_test_structure(test_root, "src_feat", "dst_feat");

        fs::write(&src_fun_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&dst_fun_rs, "").unwrap();
        fs::write(&src_fun_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_c, "int foo() { return 100; }").unwrap();

        std::env::set_current_dir(test_root).unwrap();
        Feature::sync("src_feat", "dst_feat").unwrap();

        let content = fs::read_to_string(&dst_fun_rs).unwrap();
        assert!(content.trim().is_empty());
    }

    #[test]
    fn test_sync_target_not_empty() {
        let temp_dir = TempDir::new().unwrap();
        let test_root = temp_dir.path();

        let (src_fun_rs, dst_fun_rs, src_fun_c, dst_fun_c) =
            create_sync_test_structure(test_root, "src_feat", "dst_feat");

        fs::write(&src_fun_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&dst_fun_rs, "pub fn bar() -> i32 { 100 }").unwrap();
        fs::write(&src_fun_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_c, "int foo() { return 42; }").unwrap();

        std::env::set_current_dir(test_root).unwrap();
        Feature::sync("src_feat", "dst_feat").unwrap();

        let content = fs::read_to_string(&dst_fun_rs).unwrap();
        assert_eq!(content.trim(), "pub fn bar() -> i32 { 100 }");
    }

    #[test]
    fn test_sync_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let test_root = temp_dir.path();

        let c2rust_dir = test_root.join(".c2rust");
        fs::create_dir_all(&c2rust_dir).unwrap();

        let src_feature = c2rust_dir.join("src_feat");
        let dst_feature = c2rust_dir.join("dst_feat");

        let src_mod_a = src_feature.join("rust/src/mod_a");
        let dst_mod_a = dst_feature.join("rust/src/mod_a");
        let src_mod_b = src_feature.join("rust/src/mod_b");
        let dst_mod_b = dst_feature.join("rust/src/mod_b");
        fs::create_dir_all(&src_mod_a).unwrap();
        fs::create_dir_all(&dst_mod_a).unwrap();
        fs::create_dir_all(&src_mod_b).unwrap();
        fs::create_dir_all(&dst_mod_b).unwrap();

        let src_fun_foo_a_rs = src_mod_a.join("fun_foo.rs");
        let dst_fun_foo_a_rs = dst_mod_a.join("fun_foo.rs");
        let src_fun_foo_a_c = src_mod_a.join("fun_foo.c");
        let dst_fun_foo_a_c = dst_mod_a.join("fun_foo.c");

        let src_var_bar_a_rs = src_mod_a.join("var_bar.rs");
        let dst_var_bar_a_rs = dst_mod_a.join("var_bar.rs");
        let src_var_bar_a_c = src_mod_a.join("var_bar.c");
        let dst_var_bar_a_c = dst_mod_a.join("var_bar.c");

        let src_fun_foo_b_rs = src_mod_b.join("fun_foo.rs");
        let dst_fun_foo_b_rs = dst_mod_b.join("fun_foo.rs");
        let src_fun_foo_b_c = src_mod_b.join("fun_foo.c");
        let dst_fun_foo_b_c = dst_mod_b.join("fun_foo.c");

        fs::write(&src_fun_foo_a_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&dst_fun_foo_a_rs, "").unwrap();
        fs::write(&src_fun_foo_a_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_foo_a_c, "int foo() { return 42; }").unwrap();

        fs::write(&src_var_bar_a_rs, "pub static mut BAR: i32 = 100;").unwrap();
        fs::write(&dst_var_bar_a_rs, "pub static mut BAZ: i32 = 200;").unwrap();
        fs::write(&src_var_bar_a_c, "int BAR = 100;").unwrap();
        fs::write(&dst_var_bar_a_c, "int BAR = 100;").unwrap();

        fs::write(&src_fun_foo_b_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&dst_fun_foo_b_rs, "").unwrap();
        fs::write(&src_fun_foo_b_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_foo_b_c, "int foo() { return 100; }").unwrap();

        std::env::set_current_dir(test_root).unwrap();
        Feature::sync("src_feat", "dst_feat").unwrap();

        let content_foo_a = fs::read_to_string(&dst_fun_foo_a_rs).unwrap();
        assert_eq!(content_foo_a.trim(), "pub fn foo() -> i32 { 42 }");

        let content_bar_a = fs::read_to_string(&dst_var_bar_a_rs).unwrap();
        assert_eq!(content_bar_a.trim(), "pub static mut BAZ: i32 = 200;");

        let content_foo_b = fs::read_to_string(&dst_fun_foo_b_rs).unwrap();
        assert!(content_foo_b.trim().is_empty());
    }

    #[test]
    fn test_sync_module_not_exist() {
        let temp_dir = TempDir::new().unwrap();
        let test_root = temp_dir.path();

        let c2rust_dir = test_root.join(".c2rust");
        fs::create_dir_all(&c2rust_dir).unwrap();

        let src_feature = c2rust_dir.join("src_feat");
        let dst_feature = c2rust_dir.join("dst_feat");

        let src_mod_a = src_feature.join("rust/src/mod_a");
        let src_mod_b = src_feature.join("rust/src/mod_b");
        let dst_mod_b = dst_feature.join("rust/src/mod_b");
        fs::create_dir_all(&src_mod_a).unwrap();
        fs::create_dir_all(&src_mod_b).unwrap();
        fs::create_dir_all(&dst_mod_b).unwrap();

        let src_fun_foo_b_rs = src_mod_b.join("fun_foo.rs");
        let dst_fun_foo_b_rs = dst_mod_b.join("fun_foo.rs");
        let src_fun_foo_b_c = src_mod_b.join("fun_foo.c");
        let dst_fun_foo_b_c = dst_mod_b.join("fun_foo.c");

        fs::write(&src_fun_foo_b_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&dst_fun_foo_b_rs, "").unwrap();
        fs::write(&src_fun_foo_b_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_foo_b_c, "int foo() { return 42; }").unwrap();

        std::env::set_current_dir(test_root).unwrap();
        Feature::sync("src_feat", "dst_feat").unwrap();

        let content_foo_b = fs::read_to_string(&dst_fun_foo_b_rs).unwrap();
        assert_eq!(content_foo_b.trim(), "pub fn foo() -> i32 { 42 }");
    }

    #[test]
    fn test_sync_dst_file_not_exist() {
        let temp_dir = TempDir::new().unwrap();
        let test_root = temp_dir.path();

        let (src_fun_rs, dst_fun_rs, src_fun_c, dst_fun_c) =
            create_sync_test_structure(test_root, "src_feat", "dst_feat");

        fs::write(&src_fun_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&src_fun_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_c, "int foo() { return 42; }").unwrap();
        fs::write(&dst_fun_rs, "").unwrap();
        fs::remove_file(&dst_fun_rs).unwrap();

        std::env::set_current_dir(test_root).unwrap();
        Feature::sync("src_feat", "dst_feat").unwrap();

        assert!(!dst_fun_rs.exists());
    }

    #[test]
    fn test_sync_c_file_not_exist() {
        let temp_dir = TempDir::new().unwrap();
        let test_root = temp_dir.path();

        let (src_fun_rs, dst_fun_rs, src_fun_c, dst_fun_c) =
            create_sync_test_structure(test_root, "src_feat", "dst_feat");

        fs::write(&src_fun_rs, "pub fn foo() -> i32 { 42 }").unwrap();
        fs::write(&dst_fun_rs, "").unwrap();
        fs::write(&dst_fun_c, "int foo() { return 42; }").unwrap();
        fs::write(&src_fun_c, "int foo() { return 42; }").unwrap();
        fs::remove_file(&src_fun_c).unwrap();

        std::env::set_current_dir(test_root).unwrap();
        Feature::sync("src_feat", "dst_feat").unwrap();

        assert!(!src_fun_c.exists());
        let content = fs::read_to_string(&dst_fun_rs).unwrap();
        assert!(content.trim().is_empty());
    }

    #[test]
    fn test_skip_duplicate_weak_fns_skips_weak_duplicate() {
        use crate::file::test_helpers::{make_fn_definition_node, make_translation_unit};

        let temp_dir = TempDir::new().unwrap();

        // file1: non-weak definition of "foo"
        let path1 = temp_dir.path().join("file1.c2rust");
        let root1 = make_translation_unit(vec![make_fn_definition_node("foo", false)]);

        // file2: weak definition of "foo"
        let path2 = temp_dir.path().join("file2.c2rust");
        let root2 = make_translation_unit(vec![make_fn_definition_node("foo", true)]);

        let mut feature = Feature {
            root: temp_dir.path().to_path_buf(),
            prefix: temp_dir.path().to_path_buf(),
            name: "test".to_string(),
            files: vec![
                File::new_for_test(root1, path1),
                File::new_for_test(root2, path2.clone()),
            ],
            fast: false,
        };

        feature.skip_duplicate_weak_fns().unwrap();

        // file1 should still have "foo" (non-weak)
        assert_eq!(feature.files[0].iter().len(), 1);
        assert_eq!(feature.files[0].iter()[0].kind.name(), Some("foo"));

        // file2's "foo" (weak) should be removed
        assert_eq!(feature.files[1].iter().len(), 0);
    }

    #[test]
    fn test_skip_duplicate_weak_fns_keeps_unique_weak_fn() {
        use crate::file::test_helpers::{make_fn_definition_node, make_translation_unit};

        let temp_dir = TempDir::new().unwrap();
        let path1 = temp_dir.path().join("file1.c2rust");
        // "bar" only appears once (weak), should NOT be skipped
        let root1 = make_translation_unit(vec![make_fn_definition_node("bar", true)]);

        let mut feature = Feature {
            root: temp_dir.path().to_path_buf(),
            prefix: temp_dir.path().to_path_buf(),
            name: "test".to_string(),
            files: vec![File::new_for_test(root1, path1)],
            fast: false,
        };

        feature.skip_duplicate_weak_fns().unwrap();

        // "bar" appears only once, so it should NOT be skipped
        assert_eq!(feature.files[0].iter().len(), 1);
        assert_eq!(feature.files[0].iter()[0].kind.name(), Some("bar"));
    }

    #[test]
    fn test_skip_duplicate_weak_fns_all_weak_keeps_first() {
        use crate::file::test_helpers::{make_fn_definition_node, make_translation_unit};

        let temp_dir = TempDir::new().unwrap();

        // file1: weak definition of "foo"
        let path1 = temp_dir.path().join("file1.c2rust");
        let root1 = make_translation_unit(vec![make_fn_definition_node("foo", true)]);

        // file2: also a weak definition of "foo"
        let path2 = temp_dir.path().join("file2.c2rust");
        let root2 = make_translation_unit(vec![make_fn_definition_node("foo", true)]);

        let mut feature = Feature {
            root: temp_dir.path().to_path_buf(),
            prefix: temp_dir.path().to_path_buf(),
            name: "test".to_string(),
            files: vec![
                File::new_for_test(root1, path1),
                File::new_for_test(root2, path2),
            ],
            fast: false,
        };

        feature.skip_duplicate_weak_fns().unwrap();

        // First weak "foo" should be kept
        assert_eq!(feature.files[0].iter().len(), 1);
        assert_eq!(feature.files[0].iter()[0].kind.name(), Some("foo"));

        // Second weak "foo" should be removed
        assert_eq!(feature.files[1].iter().len(), 0);
    }
}
