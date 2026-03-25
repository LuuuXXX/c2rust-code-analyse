use crate::{Feature, File, Result, ToError};
use quote::{quote, ToTokens};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::{visit_foreign_item, visit_item, visit_path, Visit};
use walkdir::WalkDir;

struct DepNames {
    used_names: HashMap<String, bool>, // bool 表示是否是 pub 签名依赖
    mac_tokens: String,
}

impl DepNames {
    fn new() -> Self {
        Self {
            used_names: HashMap::new(),
            mac_tokens: String::new(),
        }
    }

    fn contains(&self, name: &str) -> bool {
        if self.used_names.contains_key(name) {
            return true;
        }
        let regex = regex::Regex::new(&format!("[^a-zA-Z_]{name}[^a-zA-Z_]")).unwrap();
        regex.find(&self.mac_tokens).is_some()
    }

    fn mark_used(&mut self, name: String) {
        self.used_names.entry(name).or_insert(false);
    }

    fn mark_pub(&mut self, name: String) {
        self.used_names.insert(name, true);
    }

    fn is_pub(&self, name: &str) -> bool {
        self.used_names.get(name).copied().unwrap_or(false)
    }
}

impl Visit<'_> for DepNames {
    fn visit_path(&mut self, path: &syn::Path) {
        if let Some(ident) = path.segments.last() {
            let name = ident.ident.to_string();
            if !name.starts_with("_c2rust_private_") {
                self.mark_used(name);
            }
        }
        // Option<&T>形式，还需要处理T.
        visit_path(self, path);
    }

    fn visit_use_name(&mut self, name: &syn::UseName) {
        self.mark_used(name.ident.to_string());
    }

    fn visit_use_rename(&mut self, rename: &syn::UseRename) {
        self.mark_used(rename.ident.to_string());
    }

    // macro只能通过字符串匹配
    fn visit_macro(&mut self, mac: &syn::Macro) {
        self.mac_tokens.push_str(&mac.tokens.to_string());
    }
}

struct PubDepVisitor<'a>(&'a mut DepNames);

impl Visit<'_> for PubDepVisitor<'_> {
    fn visit_path(&mut self, path: &syn::Path) {
        if let Some(ident) = path.segments.last() {
            let name = ident.ident.to_string();
            if !name.starts_with("_c2rust_private_") {
                self.0.mark_pub(name);
            }
        }
        visit_path(self, path);
    }

    fn visit_block(&mut self, _: &syn::Block) {}
    fn visit_expr(&mut self, _: &syn::Expr) {}
    fn visit_stmt(&mut self, _: &syn::Stmt) {}
}

struct CollectedItems {
    named_items: HashMap<String, Vec<TypeItem>>,
    ffi_items: HashMap<String, Vec<syn::ForeignItem>>,
    foreign_mod_template: Option<syn::ItemForeignMod>,
}

struct TypeItem {
    type_def: syn::Item,
    impl_blocks: Vec<syn::ItemImpl>,
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

#[derive(Default)]
struct Duplicates {
    named_to_extract: Vec<TypeItem>,
    named_remove_set: HashSet<String>,
    ffi_to_extract: Vec<syn::ForeignItem>,
    ffi_remove_set: HashSet<String>,
}

impl Duplicates {
    fn remove(&mut self, names: &HashSet<String>) {
        self.named_to_extract.retain(|item| {
            if let Some(name) = Feature::item_name(&item.type_def) {
                if names.contains(&name) {
                    println!("{name} - remove type item defined in mod");
                    return false;
                }
            }
            true
        });
        self.ffi_to_extract.retain(|item| {
            if let Some(name) = Feature::foreign_item_name(&item) {
                if names.contains(&name) {
                    println!("{name} - remove foreign item defined in mod");
                    return false;
                }
            }
            true
        });
    }
}

impl Feature {
    /// 将每个 mod_xxx 目录下的 fun_*.rs、var_*.rs 和 mod.rs 合并为单个 mod_xxx.rs 文件
    /// mod.rs 中仅保留该模块实际依赖的类型和 FFI 声明
    /// 然后将重复的类型和 FFI 提取到 lib.rs
    pub fn merge(&mut self) -> Result<()> {
        println!("Starting merge for feature '{}'", self.name);

        for file in &self.files {
            self.merge_file(file)?;
        }

        self.deduplicate_mod_rs()?;

        self.link_src()?;

        println!("Feature '{}' merged successfully", self.name);
        Ok(())
    }

    // 合并单个 File 对应的 Rust 文件
    fn merge_file(&self, file: &File) -> Result<bool> {
        let mod_name = Self::get_mod_name_for_file(&self.prefix, file)?;
        let mod_dir = self.root.join("rust/src").join(&mod_name);

        if !mod_dir.exists() {
            return Ok(false);
        }

        println!("Processing mod for merge: {}", mod_name);

        let module_names = Self::collect_modules_from_mod_rs(&mod_dir)?;

        println!(
            "Merging {} modules for mod: {} ...",
            module_names.len(),
            mod_name
        );

        if module_names.is_empty() {
            println!("No modules to merge for: {}", mod_name);
            return Ok(false);
        }

        let mut items = Vec::new();
        let mut deps = DepNames::new();

        for module_name in &module_names {
            let rs_file = mod_dir.join(module_name).with_extension("rs");
            Self::merge_main_item(&rs_file, &mut items, &mut deps)?;
        }

        let mod_rs = mod_dir.join("mod.rs");
        let (type_items, foreign_mod) = Self::extract_dependencies(&mod_rs, &mut deps)?;

        let mut merged_items = Vec::new();
        merged_items.push(syn::parse2(quote! { use super::*; }).unwrap());

        for alias in &module_names {
            merged_items
                .push(syn::parse_str(&format!("use super::{mod_name} as {alias};")).unwrap());
        }

        for type_item in &type_items {
            if let Some(type_name) = type_item.name() {
                let mut type_def = type_item.type_def.clone();
                Self::set_item_visibility(&mut type_def, deps.is_pub(&type_name));
                merged_items.push(type_def);
                for impl_block in &type_item.impl_blocks {
                    merged_items.push(syn::Item::Impl(impl_block.clone()));
                }
            }
        }
        if let Some(fm) = foreign_mod {
            merged_items.push(syn::Item::ForeignMod(fm));
        }
        merged_items.extend(items);

        let merged_file = syn::File {
            shebang: None,
            attrs: Vec::new(),
            items: merged_items,
        };

        let formatted = prettyplease::unparse(&merged_file);

        let merged_mod_dir = self.root.join("rust/src.2");
        let _ = fs::create_dir(&merged_mod_dir);
        let merged_rs = merged_mod_dir.join(&mod_name).with_extension("rs");
        fs::write(&merged_rs, formatted.as_bytes())
            .log_err(&format!("write {}", merged_rs.display()))?;

        println!("File merged successfully: {}", merged_rs.display());

        Ok(true)
    }

    fn collect_modules_from_mod_rs(mod_dir: &Path) -> Result<Vec<String>> {
        let mod_rs_path = mod_dir.join("mod.rs");

        if !mod_rs_path.exists() {
            return Ok(vec![]);
        }

        let content =
            fs::read_to_string(&mod_rs_path).log_err(&format!("read {}", mod_rs_path.display()))?;

        let ast = syn::parse_file(&content).log_err(&format!("parse {}", mod_rs_path.display()))?;

        let mut modules = Vec::new();

        for item in ast.items {
            if let syn::Item::Mod(mod_item) = item {
                let mod_name = mod_item.ident.to_string();

                if mod_name.starts_with("fun_") || mod_name.starts_with("var_") {
                    let rs_file = mod_dir.join(&mod_name).with_extension("rs");
                    if rs_file.exists() {
                        modules.push(mod_name);
                    }
                }
            }
        }

        Ok(modules)
    }

    fn merge_main_item(
        rs_file: &Path,
        all_items: &mut Vec<syn::Item>,
        deps: &mut DepNames,
    ) -> Result<()> {
        let content =
            fs::read_to_string(rs_file).log_err(&format!("read {}", rs_file.display()))?;
        let ast = syn::parse_file(&content).log_err(&format!("parse {}", rs_file.display()))?;
        let file_name = rs_file
            .with_extension("")
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        if !file_name.starts_with("fun_") && !file_name.starts_with("var_") {
            return Ok(());
        }
        let main_item_name = &file_name[4..];
        let mut main_item = None;
        let mut other_items = Vec::new();

        for item in ast.items {
            match item {
                syn::Item::Fn(ref item_fn) if item_fn.sig.ident.to_string() == main_item_name => {
                    main_item = Some(item)
                }
                syn::Item::Static(ref item_static)
                    if item_static.ident.to_string() == main_item_name =>
                {
                    main_item = Some(item)
                }
                syn::Item::Use(ref item_use) if Self::is_use_super(item_use) => {}
                _ => other_items.push(item),
            }
        }
        if let Some(syn::Item::Fn(mut fn_item)) = main_item {
            Self::merge_item_fn(other_items, &mut fn_item)?;
            if matches!(fn_item.vis, syn::Visibility::Public(_)) {
                PubDepVisitor(deps).visit_signature(&fn_item.sig);
            }
            visit_item(deps, &syn::Item::Fn(fn_item.clone()));
            all_items.push(syn::Item::Fn(fn_item));
        } else if let Some(syn::Item::Static(mut var_item)) = main_item {
            Self::merge_item_static(other_items, &mut var_item);
            if matches!(var_item.vis, syn::Visibility::Public(_)) {
                PubDepVisitor(deps).visit_type(&var_item.ty);
            }
            visit_item(deps, &syn::Item::Static(var_item.clone()));
            all_items.push(syn::Item::Static(var_item));
        } else {
            eprintln!(
                "Failed to parse {}: not found symbole {}",
                rs_file.display(),
                main_item_name
            );
        }
        Ok(())
    }

    fn is_use_super(item_use: &syn::ItemUse) -> bool {
        if item_use.leading_colon.is_some() {
            return false;
        }
        if let syn::UseTree::Path(ref path) = item_use.tree {
            return path.ident.to_string() == "super"
                && matches!(&*path.tree, syn::UseTree::Glob(_));
        }
        false
    }

    fn merge_item_fn(items: Vec<syn::Item>, fn_item: &mut syn::ItemFn) -> Result<()> {
        if Self::remove_private_attr(&mut fn_item.attrs) {
            fn_item.vis = syn::Visibility::Inherited;
        }

        if items.is_empty() {
            return Ok(());
        }
        let block = &fn_item.block;
        let new_block = quote! {{
            #(#items)*
            #block
        }};
        fn_item.block = syn::parse2(new_block).unwrap();
        Ok(())
    }

    fn merge_item_static(items: Vec<syn::Item>, static_item: &mut syn::ItemStatic) {
        if Self::remove_private_attr(&mut static_item.attrs) {
            static_item.vis = syn::Visibility::Inherited;
        }
        if items.is_empty() {
            return;
        }
        let expr = &static_item.expr;
        let new_expr = quote! {{
            #(#items)*
            #expr
        }};
        static_item.expr = syn::parse2(new_expr).unwrap();
    }

    fn item_name(item: &syn::Item) -> Option<String> {
        match item {
            syn::Item::Struct(item) => Some(item.ident.to_string()),
            syn::Item::Union(item) => Some(item.ident.to_string()),
            syn::Item::Const(item) => Some(item.ident.to_string()),
            syn::Item::Type(item) => Some(item.ident.to_string()),
            syn::Item::Fn(item) => Some(item.sig.ident.to_string()),
            _ => None,
        }
    }

    fn foreign_item_name(item: &syn::ForeignItem) -> Option<String> {
        match item {
            syn::ForeignItem::Fn(item) => Some(item.sig.ident.to_string()),
            syn::ForeignItem::Static(item) => Some(item.ident.to_string()),
            _ => None,
        }
    }

    fn impl_self_type_name(impl_item: &syn::ItemImpl) -> Option<String> {
        match &*impl_item.self_ty {
            syn::Type::Path(type_path) if type_path.qself.is_none() => {
                if let Some(segment) = type_path.path.segments.last() {
                    Some(segment.ident.to_string())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn set_item_visibility(item: &mut syn::Item, is_pub: bool) {
        let vis = if is_pub {
            syn::parse_str("pub").unwrap()
        } else {
            syn::Visibility::Inherited
        };
        match item {
            syn::Item::Struct(s) => s.vis = vis,
            syn::Item::Union(u) => u.vis = vis,
            syn::Item::Const(c) => c.vis = vis,
            syn::Item::Type(t) => t.vis = vis,
            _ => {}
        }
    }

    fn extract_dependencies(
        mod_rs: &Path,
        deps: &mut DepNames,
    ) -> Result<(Vec<TypeItem>, Option<syn::ItemForeignMod>)> {
        let content = fs::read_to_string(mod_rs).log_err(&format!("read {}", mod_rs.display()))?;
        let ast = syn::parse_file(&content).log_err(&format!("parse {}", mod_rs.display()))?;

        let mut all_types: HashMap<String, TypeItem> = HashMap::new();
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
                    if let Some(type_name) = Self::impl_self_type_name(&impl_block) {
                        if let Some(type_item) = all_types.get_mut(&type_name) {
                            type_item.add_impl(impl_block);
                        }
                    }
                }
                _ => {
                    if let Some(name) = Self::item_name(&item) {
                        all_types.insert(name, TypeItem::new(item));
                    }
                }
            }
        }

        let mut dep_types = Vec::new();
        let mut dep_ffi = Vec::new();
        Self::filter_dependencies(all_types, all_ffi, deps, &mut dep_types, &mut dep_ffi);

        let foreign_mod = if !dep_ffi.is_empty() {
            let mut fm = foreign_mod_template.unwrap();
            fm.items = dep_ffi;
            Some(fm)
        } else {
            None
        };

        Ok((dep_types, foreign_mod))
    }

    fn filter_dependencies(
        mut all_types: HashMap<String, TypeItem>,
        all_ffi: HashMap<String, syn::ForeignItem>,
        deps: &mut DepNames,
        dep_types: &mut Vec<TypeItem>,
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
                    visit_item(deps, &type_item.type_def);
                    for impl_block in &type_item.impl_blocks {
                        visit_item(deps, &syn::Item::Impl(impl_block.clone()));
                    }

                    if deps.is_pub(name) {
                        PubDepVisitor(deps).visit_item(&type_item.type_def);
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

    fn link_src(&self) -> Result<()> {
        let src = self.root.join("rust/src");
        if src.is_symlink() {
            fs::remove_file(&src).log_err("remove link[rust/src]")?;
        } else {
            let old_src = self.root.join("rust/src.1");
            let _ = fs::remove_dir_all(&old_src);
            let _ = fs::rename(&src, &old_src).log_err("rename[src -> src.1]")?;
        }
        let new_src = self.root.join("rust/src.2");
        let _ = std::os::unix::fs::symlink(new_src, src).log_err("link[src -> src.2]")?;
        Ok(())
    }

    fn remove_private_attr(attrs: &mut Vec<syn::Attribute>) -> bool {
        let len = attrs.len();
        attrs.retain(|attr| {
            let s = attr.to_token_stream().to_string();
            !s.contains("_c2rust_private_")
        });
        len != attrs.len()
    }

    fn deduplicate_mod_rs(&self) -> Result<()> {
        let src_2 = self.root.join("rust/src.2");
        if !src_2.exists() {
            return Ok(());
        }

        let mod_files = Self::collect_mod_files(&src_2)?;
        if mod_files.is_empty() {
            return Ok(());
        }

        let collected = Self::collect_items_from_files(&mod_files)?;
        let mut duplicates = Self::find_duplicates(&collected.named_items, &collected.ffi_items);

        Self::update_lib_rs(&src_2, &mut duplicates, &collected.foreign_mod_template)?;

        if !duplicates.named_remove_set.is_empty() || !duplicates.ffi_remove_set.is_empty() {
            Self::remove_duplicates_from_files(&mod_files, &duplicates)?;
        }

        println!(
            "Deduplicated {} types and {} FFI declarations to lib.rs",
            duplicates.named_remove_set.len(),
            duplicates.ffi_remove_set.len()
        );
        Ok(())
    }

    fn collect_mod_files(src_2: &Path) -> Result<Vec<PathBuf>> {
        let mod_files: Vec<PathBuf> = WalkDir::new(src_2)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().is_file()
                    && e.path().extension().map(|ext| ext == "rs").unwrap_or(false)
                    && e.path()
                        .file_name()
                        .map(|n| n.to_string_lossy().starts_with("mod_"))
                        .unwrap_or(false)
            })
            .map(|e| e.path().to_path_buf())
            .collect();
        Ok(mod_files)
    }

    fn collect_items_from_files(mod_files: &[PathBuf]) -> Result<CollectedItems> {
        let mut named_items: HashMap<String, Vec<TypeItem>> = HashMap::new();
        let mut ffi_items: HashMap<String, Vec<syn::ForeignItem>> = HashMap::new();
        let mut foreign_mod_template: Option<syn::ItemForeignMod> = None;

        for mod_file in mod_files {
            let content =
                fs::read_to_string(mod_file).log_err(&format!("read {}", mod_file.display()))?;
            let ast =
                syn::parse_file(&content).log_err(&format!("parse {}", mod_file.display()))?;

            let mut file_type_items: Vec<(String, TypeItem)> = Vec::new();
            let mut file_impls: Vec<(String, syn::ItemImpl)> = Vec::new();

            for item in ast.items {
                match item {
                    syn::Item::Struct(s) => {
                        let name = s.ident.to_string();
                        file_type_items.push((name, TypeItem::new(syn::Item::Struct(s))));
                    }
                    syn::Item::Union(u) => {
                        let name = u.ident.to_string();
                        file_type_items.push((name, TypeItem::new(syn::Item::Union(u))));
                    }
                    syn::Item::Const(c) => {
                        let name = c.ident.to_string();
                        file_type_items.push((name, TypeItem::new(syn::Item::Const(c))));
                    }
                    syn::Item::Type(t) => {
                        let name = t.ident.to_string();
                        file_type_items.push((name, TypeItem::new(syn::Item::Type(t))));
                    }
                    syn::Item::Impl(impl_block) => {
                        if let Some(type_name) = Self::impl_self_type_name(&impl_block) {
                            file_impls.push((type_name, impl_block));
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

            for (type_name, mut type_item) in file_type_items {
                for (impl_type_name, impl_block) in &file_impls {
                    if *impl_type_name == type_name {
                        type_item.add_impl(impl_block.clone());
                    }
                }
                named_items.entry(type_name).or_default().push(type_item);
            }
        }

        Ok(CollectedItems {
            named_items,
            ffi_items,
            foreign_mod_template,
        })
    }

    fn item_body(item: &syn::Item) -> String {
        let mut item = item.clone();
        let (attrs, vis) = match item {
            syn::Item::Struct(ref mut i) => (&mut i.attrs, &mut i.vis),
            syn::Item::Union(ref mut i) => (&mut i.attrs, &mut i.vis),
            syn::Item::Type(ref mut i) => (&mut i.attrs, &mut i.vis),
            syn::Item::Const(ref mut i) => (&mut i.attrs, &mut i.vis),
            _ => return item.to_token_stream().to_string(),
        };
        attrs.clear();
        *vis = syn::Visibility::Inherited;
        item.to_token_stream().to_string()
    }

    fn find_duplicates(
        named_items: &HashMap<String, Vec<TypeItem>>,
        ffi_items: &HashMap<String, Vec<syn::ForeignItem>>,
    ) -> Duplicates {
        let mut named_to_extract: Vec<TypeItem> = Vec::new();
        let mut named_remove_set: HashSet<String> = HashSet::new();
        let mut ffi_to_extract: Vec<syn::ForeignItem> = Vec::new();
        let mut ffi_remove_set: HashSet<String> = HashSet::new();

        for (type_name, type_items) in named_items {
            if type_items.len() > 1 {
                let first_type_body = Self::item_body(&type_items[0].type_def);
                if type_items
                    .iter()
                    .all(|type_item| Self::item_body(&type_item.type_def) == first_type_body)
                {
                    named_to_extract.push(type_items[0].clone());
                    named_remove_set.insert(type_name.clone());
                }
            }
        }

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

    /// Returns true if `item` is a glob import of `core::ffi`, `::core::ffi`, or `std::ffi`
    /// (i.e. `use core::ffi::*;`, `use ::core::ffi::*;`, or `use std::ffi::*;`).
    fn is_ffi_glob_import(item: &syn::Item) -> bool {
        let syn::Item::Use(item_use) = item else {
            return false;
        };
        // must be a glob at the leaf
        let syn::UseTree::Path(root) = &item_use.tree else {
            return false;
        };
        // accept leading `::` (global path) or no leading `::`
        let crate_name = root.ident.to_string();
        if crate_name != "core" && crate_name != "std" {
            return false;
        }
        let syn::UseTree::Path(ffi_seg) = root.tree.as_ref() else {
            return false;
        };
        if ffi_seg.ident != "ffi" {
            return false;
        }
        matches!(ffi_seg.tree.as_ref(), syn::UseTree::Glob(_))
    }

    fn update_lib_rs(
        src_2: &Path,
        duplicates: &mut Duplicates,
        foreign_mod_template: &Option<syn::ItemForeignMod>,
    ) -> Result<()> {
        let lib_rs_file = src_2.parent().unwrap().join("src/lib.rs");
        let content =
            fs::read_to_string(&lib_rs_file).log_err(&format!("read {}", lib_rs_file.display()))?;
        let mut lib_rs =
            syn::parse_file(&content).log_err(&format!("parse {}", lib_rs_file.display()))?;
        let lib_items = &mut lib_rs.items;

        // Ensure `use ::core::ffi::*;` is present so that C FFI type aliases
        // (c_uchar, c_int, …) are in scope for all child modules.
        if !lib_items.iter().any(Self::is_ffi_glob_import) {
            let use_ffi: syn::Item = syn::parse_str("use ::core::ffi::*;")
                .log_err("parse use ::core::ffi::*")?;
            lib_items.insert(0, use_ffi);
        }

        // c类型，函数都在同一个名字空间.
        let mut used_names = HashSet::new();
        lib_items.iter().for_each(|item| {
            if let Some(name) = Self::item_name(item) {
                used_names.insert(name);
            }
        });
        duplicates.remove(&used_names);

        for type_item in &duplicates.named_to_extract {
            lib_items.push(type_item.type_def.clone());
            for impl_block in &type_item.impl_blocks {
                lib_items.push(syn::Item::Impl(impl_block.clone()));
            }
        }

        if !duplicates.ffi_to_extract.is_empty() {
            if let Some(mut fm) = foreign_mod_template.clone() {
                fm.items = duplicates.ffi_to_extract.clone();
                lib_items.push(syn::Item::ForeignMod(fm));
            }
        }

        let lib_content = prettyplease::unparse(&lib_rs);
        let lib_rs_path = src_2.join("lib.rs");
        fs::write(&lib_rs_path, lib_content.as_bytes())
            .log_err(&format!("write {}", lib_rs_path.display()))?;

        Ok(())
    }

    fn remove_duplicates_from_files(mod_files: &[PathBuf], duplicates: &Duplicates) -> Result<()> {
        for mod_file in mod_files {
            let content =
                fs::read_to_string(mod_file).log_err(&format!("read {}", mod_file.display()))?;
            let mut ast =
                syn::parse_file(&content).log_err(&format!("parse {}", mod_file.display()))?;

            ast.items.retain_mut(|item| match item {
                syn::Item::Struct(s) => !duplicates.named_remove_set.contains(&s.ident.to_string()),
                syn::Item::Union(u) => !duplicates.named_remove_set.contains(&u.ident.to_string()),
                syn::Item::Const(c) => !duplicates.named_remove_set.contains(&c.ident.to_string()),
                syn::Item::Type(t) => !duplicates.named_remove_set.contains(&t.ident.to_string()),
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

    fn ffi_name(item: &syn::ForeignItem) -> String {
        match item {
            syn::ForeignItem::Fn(f) => {
                Self::extract_link_name(&f.attrs).unwrap_or_else(|| f.sig.ident.to_string())
            }
            syn::ForeignItem::Static(s) => {
                Self::extract_link_name(&s.attrs).unwrap_or_else(|| s.ident.to_string())
            }
            _ => String::new(),
        }
    }

    fn extract_link_name(attrs: &[syn::Attribute]) -> Option<String> {
        for attr in attrs {
            let attr_str = attr.to_token_stream().to_string();
            if attr_str.contains("link_name") {
                if let Some(start) = attr_str.find("link_name") {
                    let rest = &attr_str[start..];
                    if let Some(quote_start) = rest.find('"') {
                        let rest = &rest[quote_start + 1..];
                        if let Some(quote_end) = rest.find('"') {
                            return Some(rest[..quote_end].to_string());
                        }
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::visit::Visit;

    #[test]
    fn test_item_name() {
        let item: syn::Item = syn::parse_str("struct MyStruct { x: i32 }").unwrap();
        assert_eq!(Feature::item_name(&item), Some("MyStruct".to_string()));

        let item: syn::Item = syn::parse_str("union MyUnion { x: i32 }").unwrap();
        assert_eq!(Feature::item_name(&item), Some("MyUnion".to_string()));

        let item: syn::Item = syn::parse_str("const MAX: usize = 100;").unwrap();
        assert_eq!(Feature::item_name(&item), Some("MAX".to_string()));

        let item: syn::Item = syn::parse_str("type MyType = i32;").unwrap();
        assert_eq!(Feature::item_name(&item), Some("MyType".to_string()));

        let item: syn::Item = syn::parse_str("fn my_func() {}").unwrap();
        assert_eq!(Feature::item_name(&item), Some("my_func".to_string()));
    
        let item: syn::Item = syn::parse_str("use std::ffi::*;").unwrap();
        assert_eq!(Feature::item_name(&item), None);
    }

    #[test]
    fn test_foreign_item_name() {
        let item: syn::ForeignItem = syn::parse_str("fn external_func(x: i32) -> i32;").unwrap();
        assert_eq!(
            Feature::foreign_item_name(&item),
            Some("external_func".to_string())
        );

        let item: syn::ForeignItem = syn::parse_str("static EXTERNAL_VAR: i32;").unwrap();
        assert_eq!(
            Feature::foreign_item_name(&item),
            Some("EXTERNAL_VAR".to_string())
        );

        let item: syn::ForeignItem = syn::parse_str("type c_int = i32;").unwrap();
        assert_eq!(Feature::foreign_item_name(&item), None);
    }

    #[test]
    fn test_is_use_super() {
        let item_use: syn::ItemUse = syn::parse_str("use super::*;").unwrap();
        assert!(Feature::is_use_super(&item_use));

        let item_use: syn::ItemUse = syn::parse_str("use super::SomeType;").unwrap();
        assert!(!Feature::is_use_super(&item_use));

        let item_use: syn::ItemUse = syn::parse_str("use crate::some_mod::*;").unwrap();
        assert!(!Feature::is_use_super(&item_use));

        let item_use: syn::ItemUse = syn::parse_str("use ::super::*;").unwrap();
        assert!(!Feature::is_use_super(&item_use));
    }

    #[test]
    fn test_remove_private_attr() {
        let mut fn_item: syn::ItemFn =
            syn::parse_str("#[_c2rust_private_abc] fn test() {}").unwrap();
        assert!(Feature::remove_private_attr(&mut fn_item.attrs));
        assert!(fn_item.attrs.is_empty());

        let mut fn_item: syn::ItemFn = syn::parse_str("#[inline] fn test() {}").unwrap();
        assert!(!Feature::remove_private_attr(&mut fn_item.attrs));
        assert_eq!(fn_item.attrs.len(), 1);

        let mut fn_item: syn::ItemFn =
            syn::parse_str("#[_c2rust_private_abc] #[inline] fn test() {}").unwrap();
        assert!(Feature::remove_private_attr(&mut fn_item.attrs));
        assert_eq!(fn_item.attrs.len(), 1);
    }

    #[test]
    fn test_merge_item_static() {
        let items = vec![syn::parse_str("const HELPER: i32 = 1;").unwrap()];
        let mut static_item: syn::ItemStatic = syn::parse_str("static mut VAR: i32 = 42;").unwrap();

        Feature::merge_item_static(items, &mut static_item);

        let code = quote::quote!(#static_item).to_string();
        assert!(code.contains("HELPER"));
        assert!(code.contains("42"));
    }

    #[test]
    fn test_dep_names_visit_path() {
        let mut deps = DepNames::new();

        let path: syn::Path = syn::parse_str("SomeType").unwrap();
        deps.visit_path(&path);
        assert!(deps.contains("SomeType"));
        assert!(!deps.contains("OtherType"));

        let path: syn::Path = syn::parse_str("_c2rust_private_hidden").unwrap();
        deps.visit_path(&path);
        assert!(!deps.contains("_c2rust_private_hidden"));
    }

    #[test]
    fn test_dep_names_visit_macro() {
        let mut deps = DepNames::new();

        let mac: syn::Macro = syn::parse_str("some_macro!(x SomeType y OtherType z)").unwrap();
        deps.visit_macro(&mac);
        assert!(deps.mac_tokens.contains("SomeType"));
        assert!(deps.mac_tokens.contains("OtherType"));

        // contains 使用正则 [^a-zA-Z_]{name}[^a-zA-Z_] 匹配
        // 所以需要前后有非字母字符才能匹配
        assert!(deps.contains("SomeType"));
        assert!(deps.contains("OtherType"));
    }

    #[test]
    fn test_filter_dependencies_basic() {
        let mut deps = DepNames::new();
        deps.used_names.insert("UsedType".to_string(), false);
        deps.used_names.insert("used_func".to_string(), false);

        let mut all_types: HashMap<String, TypeItem> = HashMap::new();
        all_types.insert(
            "UsedType".to_string(),
            TypeItem::new(syn::parse_str("struct UsedType { x: i32 }").unwrap()),
        );
        all_types.insert(
            "UnusedType".to_string(),
            TypeItem::new(syn::parse_str("struct UnusedType { x: i32 }").unwrap()),
        );

        let mut all_ffi: HashMap<String, syn::ForeignItem> = HashMap::new();
        all_ffi.insert(
            "used_func".to_string(),
            syn::parse_str("fn used_func() -> i32;").unwrap(),
        );
        all_ffi.insert(
            "unused_func".to_string(),
            syn::parse_str("fn unused_func() -> i32;").unwrap(),
        );

        let mut dep_types = Vec::new();
        let mut dep_ffi = Vec::new();

        Feature::filter_dependencies(all_types, all_ffi, &mut deps, &mut dep_types, &mut dep_ffi);

        assert_eq!(dep_types.len(), 1);
        assert_eq!(dep_ffi.len(), 1);
        assert_eq!(dep_types[0].name(), Some("UsedType".to_string()));
        assert_eq!(
            Feature::foreign_item_name(&dep_ffi[0]),
            Some("used_func".to_string())
        );
    }

    #[test]
    fn test_filter_dependencies_transitive() {
        let mut deps = DepNames::new();
        deps.used_names.insert("TypeA".to_string(), false);

        let mut all_types: HashMap<String, TypeItem> = HashMap::new();
        all_types.insert(
            "TypeA".to_string(),
            TypeItem::new(syn::parse_str("struct TypeA { b: TypeB }").unwrap()),
        );
        all_types.insert(
            "TypeB".to_string(),
            TypeItem::new(syn::parse_str("struct TypeB { x: i32 }").unwrap()),
        );
        all_types.insert(
            "TypeC".to_string(),
            TypeItem::new(syn::parse_str("struct TypeC { x: i32 }").unwrap()),
        );

        let all_ffi: HashMap<String, syn::ForeignItem> = HashMap::new();
        let mut dep_types = Vec::new();
        let mut dep_ffi = Vec::new();

        Feature::filter_dependencies(all_types, all_ffi, &mut deps, &mut dep_types, &mut dep_ffi);

        assert_eq!(dep_types.len(), 2);
        let names: std::collections::HashSet<_> =
            dep_types.iter().filter_map(|t| t.name()).collect();
        assert!(names.contains("TypeA"));
        assert!(names.contains("TypeB"));
        assert!(!names.contains("TypeC"));
    }

    #[test]
    fn test_ffi_name_fn() {
        let ffi: syn::ForeignItem = syn::parse_str("fn external_func(x: i32) -> i32;").unwrap();
        assert_eq!(Feature::ffi_name(&ffi), "external_func");

        let ffi: syn::ForeignItem =
            syn::parse_str(r#"#[link_name = "actual_name"] fn renamed_func() -> void;"#).unwrap();
        assert_eq!(Feature::ffi_name(&ffi), "actual_name");
    }

    #[test]
    fn test_ffi_name_static() {
        let ffi: syn::ForeignItem = syn::parse_str("static EXTERNAL_VAR: i32;").unwrap();
        assert_eq!(Feature::ffi_name(&ffi), "EXTERNAL_VAR");

        let ffi: syn::ForeignItem =
            syn::parse_str(r#"#[link_name = "actual_var"] static RENAMED_VAR: i32;"#).unwrap();
        assert_eq!(Feature::ffi_name(&ffi), "actual_var");
    }

    #[test]
    fn test_ffi_name_type() {
        let ffi: syn::ForeignItem = syn::parse_str("type c_int = i32;").unwrap();
        assert_eq!(Feature::ffi_name(&ffi), "");
    }

    #[test]
    fn test_extract_link_name() {
        let fn_item: syn::ItemFn =
            syn::parse_str(r#"#[link_name = "actual_name"] fn test() {}"#).unwrap();
        let link_name = Feature::extract_link_name(&fn_item.attrs);
        assert_eq!(link_name, Some("actual_name".to_string()));

        let fn_item: syn::ItemFn = syn::parse_str(r#"#[inline] fn test() {}"#).unwrap();
        let link_name = Feature::extract_link_name(&fn_item.attrs);
        assert_eq!(link_name, None);

        let fn_item: syn::ItemFn =
            syn::parse_str(r#"#[cfg(test)] #[link_name = "another_name"] fn test() {}"#).unwrap();
        let link_name = Feature::extract_link_name(&fn_item.attrs);
        assert_eq!(link_name, Some("another_name".to_string()));
    }

    #[test]
    fn test_collect_modules_from_mod_rs_basic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        fs::write(&mod_rs, "mod fun_foo;\nmod var_bar;").unwrap();

        fs::write(mod_dir.join("fun_foo.rs"), "pub fn foo() {}").unwrap();
        fs::write(mod_dir.join("var_bar.rs"), "pub static BAR: i32 = 42;").unwrap();

        let modules = Feature::collect_modules_from_mod_rs(&mod_dir).unwrap();
        assert_eq!(modules.len(), 2);
        assert!(modules.contains(&"fun_foo".to_string()));
        assert!(modules.contains(&"var_bar".to_string()));
    }

    #[test]
    fn test_collect_modules_from_mod_rs_only_fun() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        fs::write(&mod_rs, "mod fun_foo;\nmod fun_baz;").unwrap();

        fs::write(mod_dir.join("fun_foo.rs"), "pub fn foo() {}").unwrap();
        fs::write(mod_dir.join("fun_baz.rs"), "pub fn baz() {}").unwrap();

        let modules = Feature::collect_modules_from_mod_rs(&mod_dir).unwrap();
        assert_eq!(modules.len(), 2);
        assert!(modules.contains(&"fun_foo".to_string()));
        assert!(modules.contains(&"fun_baz".to_string()));
        assert!(!modules.contains(&"var_bar".to_string()));
    }

    #[test]
    fn test_collect_modules_from_mod_rs_only_var() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        fs::write(&mod_rs, "mod var_foo;\nmod var_bar;").unwrap();

        fs::write(mod_dir.join("var_foo.rs"), "pub static FOO: i32 = 1;").unwrap();
        fs::write(mod_dir.join("var_bar.rs"), "pub static BAR: i32 = 2;").unwrap();

        let modules = Feature::collect_modules_from_mod_rs(&mod_dir).unwrap();
        assert_eq!(modules.len(), 2);
        assert!(modules.contains(&"var_foo".to_string()));
        assert!(modules.contains(&"var_bar".to_string()));
        assert!(!modules.contains(&"fun_foo".to_string()));
    }

    #[test]
    fn test_collect_modules_from_mod_rs_empty() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        fs::write(&mod_rs, "// no modules").unwrap();

        let modules = Feature::collect_modules_from_mod_rs(&mod_dir).unwrap();
        assert_eq!(modules.len(), 0);
    }

    #[test]
    fn test_collect_modules_from_mod_rs_other_mods() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        fs::write(&mod_rs, "mod fun_foo;\nmod types;\nmod helpers;").unwrap();

        fs::write(mod_dir.join("fun_foo.rs"), "pub fn foo() {}").unwrap();
        fs::write(mod_dir.join("types.rs"), "pub struct Foo;").unwrap();
        fs::write(mod_dir.join("helpers.rs"), "pub fn helper() {}").unwrap();

        let modules = Feature::collect_modules_from_mod_rs(&mod_dir).unwrap();
        assert_eq!(modules.len(), 1);
        assert!(modules.contains(&"fun_foo".to_string()));
        assert!(!modules.contains(&"types".to_string()));
        assert!(!modules.contains(&"helpers".to_string()));
    }

    #[test]
    fn test_collect_modules_from_mod_rs_missing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        fs::write(&mod_rs, "mod fun_foo;\nmod var_bar;").unwrap();

        fs::write(mod_dir.join("fun_foo.rs"), "pub fn foo() {}").unwrap();

        let modules = Feature::collect_modules_from_mod_rs(&mod_dir).unwrap();
        assert_eq!(modules.len(), 1);
        assert!(modules.contains(&"fun_foo".to_string()));
        assert!(!modules.contains(&"var_bar".to_string()));
    }

    #[test]
    fn test_collect_modules_from_mod_rs_no_mod_rs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let modules = Feature::collect_modules_from_mod_rs(&mod_dir).unwrap();
        assert_eq!(modules.len(), 0);
    }

    #[test]
    fn test_collect_modules_from_mod_rs_invalid_syntax() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mod_dir = temp_dir.path().join("mod_test");
        fs::create_dir_all(&mod_dir).unwrap();

        let mod_rs = mod_dir.join("mod.rs");
        fs::write(&mod_rs, "mod fun_foo\nmod var_bar;").unwrap();

        let result = Feature::collect_modules_from_mod_rs(&mod_dir);
        assert!(result.is_err());
    }

    #[test]
    fn test_impl_self_type_name() {
        let impl_block: syn::ItemImpl =
            syn::parse_str("impl MyStruct { fn foo(&self) {} }").unwrap();
        assert_eq!(
            Feature::impl_self_type_name(&impl_block),
            Some("MyStruct".to_string())
        );

        let impl_block: syn::ItemImpl = syn::parse_str("impl MyType { fn bar(&self) {} }").unwrap();
        assert_eq!(
            Feature::impl_self_type_name(&impl_block),
            Some("MyType".to_string())
        );

        let impl_block: syn::ItemImpl =
            syn::parse_str("impl<T> MyGeneric<T> { fn baz(&self) {} }").unwrap();
        assert_eq!(
            Feature::impl_self_type_name(&impl_block),
            Some("MyGeneric".to_string())
        );
    }

    #[test]
    fn test_pub_dep_visitor_basic() {
        let mut deps = DepNames::new();
        PubDepVisitor(&mut deps)
            .visit_signature(&syn::parse_str("fn test(a: MyType) -> i32").unwrap());
        assert!(deps.is_pub("MyType"));
        assert!(!deps.is_pub("OtherType"));
    }

    #[test]
    fn test_pub_dep_visitor_indirect() {
        let mut deps = DepNames::new();
        deps.used_names.insert("InnerType".to_string(), false);
        deps.used_names.insert("OuterType".to_string(), true);
        PubDepVisitor(&mut deps)
            .visit_item(&syn::parse_str("struct OuterType { inner: InnerType }").unwrap());
        assert!(deps.is_pub("InnerType"));
        assert!(deps.is_pub("OuterType"));
    }

    #[test]
    fn test_set_item_visibility_struct() {
        let mut item: syn::Item = syn::parse_str("struct MyStruct { x: i32 }").unwrap();
        Feature::set_item_visibility(&mut item, true);
        if let syn::Item::Struct(s) = item {
            assert!(matches!(s.vis, syn::Visibility::Public(_)));
        } else {
            panic!("Expected struct");
        }
    }

    #[test]
    fn test_set_item_visibility_union() {
        let mut item: syn::Item = syn::parse_str("union MyUnion { x: i32 }").unwrap();
        Feature::set_item_visibility(&mut item, true);
        if let syn::Item::Union(u) = item {
            assert!(matches!(u.vis, syn::Visibility::Public(_)));
        } else {
            panic!("Expected union");
        }
    }

    #[test]
    fn test_set_item_visibility_const() {
        let mut item: syn::Item = syn::parse_str("const MAX: usize = 100;").unwrap();
        Feature::set_item_visibility(&mut item, false);
        if let syn::Item::Const(c) = item {
            assert!(matches!(c.vis, syn::Visibility::Inherited));
        } else {
            panic!("Expected const");
        }
    }

    #[test]
    fn test_set_item_visibility_type() {
        let mut item: syn::Item = syn::parse_str("type MyType = i32;").unwrap();
        Feature::set_item_visibility(&mut item, false);
        if let syn::Item::Type(t) = item {
            assert!(matches!(t.vis, syn::Visibility::Inherited));
        } else {
            panic!("Expected type");
        }
    }

    #[test]
    fn test_is_pub() {
        let mut deps = DepNames::new();
        deps.used_names.insert("PubType".to_string(), true);
        deps.used_names.insert("PrivateType".to_string(), false);
        assert!(deps.is_pub("PubType"));
        assert!(!deps.is_pub("PrivateType"));
        assert!(!deps.is_pub("UnknownType"));
    }

    #[test]
    fn test_deduplicate_basic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let src_2 = temp_dir.path().join("src.2");
        fs::create_dir_all(&src_2).unwrap();

        let mod_a = src_2.join("mod_a.rs");
        let mod_b = src_2.join("mod_b.rs");

        fs::write(&mod_a, "struct MyStruct { x: i32 }").unwrap();
        fs::write(&mod_b, "struct MyStruct { x: i32 }").unwrap();

        let mod_files = vec![mod_a.clone(), mod_b.clone()];
        let collected = Feature::collect_items_from_files(&mod_files).unwrap();
        let duplicates = Feature::find_duplicates(&collected.named_items, &collected.ffi_items);

        assert_eq!(duplicates.named_remove_set.len(), 1);
        assert!(duplicates.named_remove_set.contains("MyStruct"));
    }

    #[test]
    fn test_is_ffi_glob_import_matches() {
        for src in &[
            "use ::core::ffi::*;",
            "use core::ffi::*;",
            "use std::ffi::*;",
        ] {
            let item: syn::Item = syn::parse_str(src).unwrap();
            assert!(Feature::is_ffi_glob_import(&item), "{} should match", src);
        }
    }

    #[test]
    fn test_is_ffi_glob_import_no_match() {
        for src in &[
            "use core::ffi::c_int;",
            "use std::ffi;",
            "use core::*;",
            "use super::*;",
            "use crate::ffi::*;",
            "mod ffi {}",
        ] {
            let item: syn::Item = syn::parse_str(src).unwrap();
            assert!(!Feature::is_ffi_glob_import(&item), "{} should not match", src);
        }
    }

    #[test]
    fn test_update_lib_rs_inserts_ffi_import() {
        let temp_dir = tempfile::tempdir().unwrap();

        // Set up: rust/src/lib.rs (no ffi import), rust/src.2/ directory
        let src_dir = temp_dir.path().join("src");
        let src_2_dir = temp_dir.path().join("src.2");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&src_2_dir).unwrap();

        let lib_rs_src = src_dir.join("lib.rs");
        fs::write(
            &lib_rs_src,
            "#![allow(unused_imports)]\nmod mod_cJSON;\n",
        )
        .unwrap();

        let mut duplicates = Duplicates::default();
        Feature::update_lib_rs(&src_2_dir, &mut duplicates, &None).unwrap();

        let out = fs::read_to_string(src_2_dir.join("lib.rs")).unwrap();
        assert!(
            out.contains("use ::core::ffi::*"),
            "output should contain `use ::core::ffi::*`; got:\n{out}"
        );
        // Must appear exactly once
        assert_eq!(
            out.matches("use ::core::ffi::*").count(),
            1,
            "import should appear exactly once"
        );
    }

    #[test]
    fn test_update_lib_rs_idempotent_with_existing_import() {
        let temp_dir = tempfile::tempdir().unwrap();

        let src_dir = temp_dir.path().join("src");
        let src_2_dir = temp_dir.path().join("src.2");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&src_2_dir).unwrap();

        // lib.rs already has the import
        let lib_rs_src = src_dir.join("lib.rs");
        fs::write(
            &lib_rs_src,
            "#![allow(unused_imports)]\nuse ::core::ffi::*;\nmod mod_cJSON;\n",
        )
        .unwrap();

        let mut duplicates = Duplicates::default();
        Feature::update_lib_rs(&src_2_dir, &mut duplicates, &None).unwrap();

        let out = fs::read_to_string(src_2_dir.join("lib.rs")).unwrap();
        assert!(
            out.contains("use ::core::ffi::*"),
            "output should contain `use ::core::ffi::*`"
        );
        assert_eq!(
            out.matches("use ::core::ffi::*").count(),
            1,
            "import should appear exactly once (idempotent)"
        );
    }

    #[test]
    fn test_update_lib_rs_idempotent_with_core_ffi_import() {
        let temp_dir = tempfile::tempdir().unwrap();

        let src_dir = temp_dir.path().join("src");
        let src_2_dir = temp_dir.path().join("src.2");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&src_2_dir).unwrap();

        // lib.rs has `use core::ffi::*;` (no leading `::`)
        let lib_rs_src = src_dir.join("lib.rs");
        fs::write(
            &lib_rs_src,
            "#![allow(unused_imports)]\nuse core::ffi::*;\nmod mod_cJSON;\n",
        )
        .unwrap();

        let mut duplicates = Duplicates::default();
        Feature::update_lib_rs(&src_2_dir, &mut duplicates, &None).unwrap();

        let out = fs::read_to_string(src_2_dir.join("lib.rs")).unwrap();
        // No duplicate import should be inserted
        let ffi_count = out.matches("ffi::*").count();
        assert_eq!(ffi_count, 1, "should have exactly one ffi::* glob; got:\n{out}");
    }
}
