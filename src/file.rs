use crate::{get_clang, Error, Result, ToError};
use clang_ast::{BareSourceLocation, SourceLocation, SourceRange};
use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub type Node = clang_ast::Node<Kind>;

fn read_file_range(path: &Path, start: usize, end: usize) -> Result<String> {
    let file = fs::File::open(path).log_err(&format!("open {}", path.display()))?;
    let mmap = unsafe { memmap2::Mmap::map(&file).map_err(|_| Error::last())? };
    if start >= mmap.len() || end <= start {
        return Ok(String::new());
    }
    let bytes = &mmap[start..end.min(mmap.len())];
    Ok(String::from_utf8_lossy(bytes).to_string())
}

fn read_c_code(root: &Path, range: &SourceRange) -> Result<String> {
    let Some(ref beg) = range.begin.expansion_loc else {
        return Ok(String::new());
    };
    let Some(ref end) = range.end.expansion_loc else {
        return Ok(String::new());
    };
    read_file_range(&root.join(&*beg.file), beg.offset, end.offset + end.tok_len)
}

fn read_before(root: &Path, pos: &SourceRange) -> Result<String> {
    let Some(ref pos) = pos.begin.expansion_loc else {
        return Ok(String::new());
    };
    read_file_range(&root.join(&*pos.file), 0, pos.offset)
}

fn read_after(root: &Path, pos: &SourceRange) -> Result<String> {
    let Some(ref pos) = pos.end.expansion_loc else {
        return Ok(String::new());
    };
    read_file_range(&root.join(&*pos.file), pos.offset + pos.tok_len, usize::MAX)
}

fn read_between(root: &Path, beg: &SourceRange, end: &SourceRange) -> Result<String> {
    let Some(ref beg_loc) = beg.end.expansion_loc else {
        return read_before(root, end);
    };
    let Some(ref end_loc) = end.begin.expansion_loc else {
        return read_after(root, beg);
    };
    if end_loc.file == beg_loc.file {
        read_file_range(
            &root.join(&*beg_loc.file),
            beg_loc.offset + beg_loc.tok_len,
            end_loc.offset,
        )
    } else {
        Ok(read_after(root, beg)? + &read_before(root, end)?)
    }
}

fn read_code_between(
    root: &Path,
    beg: Option<&SourceRange>,
    end: Option<&SourceRange>,
) -> Result<String> {
    match (beg, end) {
        (Some(beg), Some(end)) => read_between(root, beg, end),
        (None, Some(end)) => read_before(root, end),
        (Some(beg), None) => read_after(root, beg),
        _ => Ok(String::new()),
    }
}

fn range_include(range: &SourceRange, included: &SourceRange) -> bool {
    let (Some(beg1), Some(beg2)) = (
        range.begin.expansion_loc.as_ref(),
        included.begin.expansion_loc.as_ref(),
    ) else {
        return false;
    };
    let (Some(end1), Some(end2)) = (
        range.end.expansion_loc.as_ref(),
        included.end.expansion_loc.as_ref(),
    ) else {
        return false;
    };
    beg1.file == beg2.file && beg1.offset <= beg2.offset && end1.offset >= end2.offset
}

fn remove_unused_attrs(code: &mut String) {
    let mut off = 0;
    let re = regex::Regex::new(r"__attribute__\s*\(\s*\(\s*always_inline\s*\)\s*\)").unwrap();
    while let Some(m) = re.find(&code[off..]) {
        off += m.start();
        code.replace_range(off..off + m.len(), "");
    }
    off = 0;
    let re = regex::Regex::new(r"__attribute__\s*\(\s*\(\s*__malloc__\s*(\(.+\))?\)\s*\)").unwrap();
    while let Some(m) = re.find(&code[off..]) {
        off += m.start();
        code.replace_range(off..off + m.len(), "");
    }
    // __gnu_inline__ 在某些场景（尤其是声明）会被编译器忽略，从而触发 -Werror=attributes
    let mut off = 0;
    let re = regex::Regex::new(r"__attribute__\s*\(\s*\(\s*[^)]*__gnu_inline__[^)]*\)\s*\)").unwrap();
    while let Some(m) = re.find(&code[off..]) {
        off += m.start();
        code.replace_range(off..off + m.len(), "");
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum Kind {
    EnumDecl(EnumDecl),
    RecordDecl(RecordDecl),
    FunctionDecl(FunctionDecl),
    VarDecl(VarDecl),
    TypedefDecl(TypedefDecl),
    TranslationUnitDecl(TranslationUnitDecl),
    CompoundStmt, // 函数需要依赖它判断是否是声明.
    Other(OtherDecl),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OtherDecl {
    kind: Option<String>,
}

impl Kind {
    fn loc(&self) -> Option<&SourceLocation> {
        match self {
            Kind::EnumDecl(ref item) => Some(&item.loc),
            Kind::RecordDecl(ref item) => Some(&item.loc),
            Kind::FunctionDecl(ref item) => Some(&item.loc),
            Kind::VarDecl(ref item) => Some(&item.loc),
            Kind::TypedefDecl(ref item) => Some(&item.loc),
            _ => None,
        }
    }
    fn loc_mut(&mut self) -> Option<&mut SourceLocation> {
        match self {
            Kind::EnumDecl(ref mut item) => Some(&mut item.loc),
            Kind::RecordDecl(ref mut item) => Some(&mut item.loc),
            Kind::FunctionDecl(ref mut item) => Some(&mut item.loc),
            Kind::VarDecl(ref mut item) => Some(&mut item.loc),
            Kind::TypedefDecl(ref mut item) => Some(&mut item.loc),
            _ => None,
        }
    }
    fn range(&self) -> Option<&SourceRange> {
        match self {
            Kind::EnumDecl(ref item) => Some(&item.range),
            Kind::RecordDecl(ref item) => Some(&item.range),
            Kind::FunctionDecl(ref item) => Some(&item.range),
            Kind::VarDecl(ref item) => Some(&item.range),
            Kind::TypedefDecl(ref item) => Some(&item.range),
            _ => None,
        }
    }
    fn set_skip(&mut self) {
        match self {
            Kind::EnumDecl(ref mut item) => item.skip = true,
            Kind::RecordDecl(ref mut item) => item.skip = true,
            Kind::FunctionDecl(ref mut item) => item.skip = true,
            Kind::VarDecl(ref mut item) => item.skip = true,
            Kind::TypedefDecl(ref mut item) => item.skip = true,
            _ => {}
        }
    }

    pub fn skip(&self) -> bool {
        match self {
            Kind::EnumDecl(ref item) => item.skip,
            Kind::RecordDecl(ref item) => item.skip,
            Kind::FunctionDecl(ref item) => item.skip,
            Kind::VarDecl(ref item) => item.skip,
            Kind::TypedefDecl(ref item) => item.skip,
            _ => true,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Kind::EnumDecl(ref item) => item.name.as_deref(),
            Kind::RecordDecl(ref item) => item.name.as_deref(),
            Kind::FunctionDecl(ref item) => Some(item.name.as_str()),
            Kind::VarDecl(ref item) => Some(item.name.as_str()),
            Kind::TypedefDecl(ref item) => Some(item.name.as_str()),
            _ => None,
        }
    }

    pub fn is_fun_declare(&self, inner: &[Node]) -> bool {
        !inner.iter().any(|e| matches!(e.kind, Kind::CompoundStmt))
    }

    pub fn is_const_var(&self) -> bool {
        let Kind::VarDecl(var) = self else {
            return false;
        };
        var.ty.is_const()
    }

    // root: $C2RUST_PROJECT_ROOT/.c2rust/c
    pub fn c_code(&self, root: &Path) -> Result<String> {
        if self.skip() {
            return Ok(String::new());
        }
        let Some(range) = self.range() else {
            return Err(Error::inval());
        };

        let mut code = read_c_code(root, range)?;

        if code.is_empty() {
            return Ok(code);
        }

        if !self.is_static() && !self.is_inline() {
            return Ok(code);
        }

        let Some(global_name) = self.global_name() else {
            eprintln!("empty global name: {}", code);
            return Err(Error::general());
        };

        let (beg, end) = name_range(self.loc().unwrap(), self.range().unwrap());
        code.replace_range(beg..end, global_name);

        // 构建工具可能控制缺省可见性，这里需要显示增加
        // 但必须先清理掉原本的 hidden/default 等，避免冲突
        strip_visibility_attrs(&mut code)?;
        code.insert_str(0, "__attribute__((visibility(\"default\"))) ");

        let re = regex::Regex::new(r"^static\s|\sstatic\s").map_err(|_| Error::inval())?;
        if let Some(m) = re.find(&code) {
            code.replace_range(m.start()..m.start() + m.len(), " ");
        }
        if !self.is_inline() {
            return Ok(code);
        }
        let re = regex::Regex::new(r"\s_*inline_*\s").map_err(|_| Error::inval())?;
        if let Some(m) = re.find(&code) {
            code.replace_range(m.start()..m.start() + m.len(), " ");
        }
        Ok(code)
    }

    // 部分attribute在类型定义之后，这部分clang生成的json文件中不包括，但是必须的不可能省略，否则可能导致编译问题
    fn tail_code(&self, root: &Path, end: Option<&SourceRange>) -> Result<String> {
        let code = read_code_between(root, self.range(), end)?;
        Ok(code)
    }

    fn rename_macro(&self) -> Option<String> {
        if !self.is_static() && !self.is_inline() {
            return None;
        }
        let Some(global_name) = self.global_name() else {
            eprintln!("empty global name: {:?}", self.name());
            return None;
        };
        let name = self.name()?;
        Some(format!(
            r##"
        #if !defined({name})
            #define {name} {global_name}
        #endif
        "##
        ))
    }

    pub fn is_inline(&self) -> bool {
        let Kind::FunctionDecl(ref item) = self else {
            return false;
        };
        item.inline
    }

    pub fn is_static(&self) -> bool {
        let storage_class = match self {
            Kind::FunctionDecl(ref item) => &item.storage_class,
            Kind::VarDecl(ref item) => &item.storage_class,
            _ => return false,
        };
        matches!(storage_class.as_deref(), Some("static"))
    }

    pub fn global_name(&self) -> Option<&str> {
        match self {
            Kind::FunctionDecl(ref item) => item.global_name.as_deref(),
            Kind::VarDecl(ref item) => item.global_name.as_deref(),
            _ => None,
        }
    }

    pub fn set_global_name(&mut self, global_name: String) {
        match self {
            Kind::FunctionDecl(ref mut item) => item.global_name = Some(global_name),
            Kind::VarDecl(ref mut item) => item.global_name = Some(global_name),
            _ => {}
        }
    }

    pub fn is_extern(&self) -> bool {
        let storage_class = match self {
            Kind::VarDecl(ref item) => &item.storage_class,
            _ => return false,
        };
        matches!(storage_class.as_ref().map(|s| s.as_str()), Some("extern"))
    }

    pub fn has_committed(&self) -> bool {
        match self {
            Kind::FunctionDecl(ref item) => item.git_commit,
            Kind::VarDecl(ref item) => item.git_commit,
            _ => false,
        }
    }

    pub fn set_git_commit(&mut self, committed: bool) {
        match self {
            Kind::FunctionDecl(ref mut item) => item.git_commit = committed,
            Kind::VarDecl(ref mut item) => item.git_commit = committed,
            _ => {}
        };
    }

    /// 变量中应该翻译带初始化值的那一个, 如果都没有初始化，且非`extern`需要任意翻译一个.
    pub fn is_inited(&self) -> bool {
        match self {
            Kind::VarDecl(ref item) => item.init.is_some() || item.fake_init,
            _ => false,
        }
    }

    pub fn is_fake_inited(&self) -> bool {
        match self {
            Kind::VarDecl(ref item) => item.fake_init,
            _ => false,
        }
    }

    /// 是否变长参数，这类函数Rust无法实现
    pub fn is_variadic(&self) -> bool {
        let Kind::FunctionDecl(ref item) = self else {
            return false;
        };
        item.ty.typedef().contains("...") || item.ty.typedef().contains("struct __va_list_tag")
    }
}

fn init_base_location(target: &mut BareSourceLocation, src: &BareSourceLocation) {
    if let (None, Some(_), Some(line)) = (
        target.presumed_file.as_ref(),
        src.presumed_file.as_ref(),
        src.presumed_line,
    ) {
        target.presumed_file = src.presumed_file.clone();
        target.presumed_line = Some(line + (target.line - src.line))
    }
}

fn init_loc(target: &mut SourceLocation, src: &SourceLocation) {
    if let (Some(ref mut target), Some(src)) =
        (target.spelling_loc.as_mut(), src.spelling_loc.as_ref())
    {
        init_base_location(target, src);
    }
    if let (Some(ref mut target), Some(src)) =
        (target.expansion_loc.as_mut(), src.expansion_loc.as_ref())
    {
        init_base_location(target, src);
    }
}

fn name_range(loc: &SourceLocation, range: &SourceRange) -> (usize, usize) {
    let (Some(name_loc), Some(beg_loc)) = (
        loc.expansion_loc.as_ref(),
        range.begin.expansion_loc.as_ref(),
    ) else {
        return (0, 0);
    };
    let offset = name_loc.offset - beg_loc.offset;
    (offset, offset + name_loc.tok_len)
}

fn strip_visibility_attrs(code: &mut String) -> Result<()> {
    // 删除 __attribute__((__visibility__("...")))
    let re = regex::Regex::new(
        r#"__attribute__\s*\(\s*\(\s*__visibility__\s*\(\s*"[^"]*"\s*\)\s*\)\s*\)\s*"#,
    )
    .map_err(|_| Error::inval())?;
    while let Some(m) = re.find(code) {
        code.replace_range(m.start()..m.end(), "");
    }

    // 删除 __attribute__((visibility("...")))
    let re = regex::Regex::new(
        r#"__attribute__\s*\(\s*\(\s*visibility\s*\(\s*"[^"]*"\s*\)\s*\)\s*\)\s*"#,
    )
    .map_err(|_| Error::inval())?;
    while let Some(m) = re.find(code) {
        code.replace_range(m.start()..m.end(), "");
    }

    Ok(())
}

fn ensure_extern_decl(code: &mut String) -> Result<()> {
    let has_extern = regex::Regex::new(r"(^|[^\w])extern([^\w]|$)")
        .map_err(|_| Error::inval())?
        .is_match(code);

    if !has_extern {
        code.insert_str(0, "extern ");
    }
    Ok(())
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TranslationUnitDecl {
    md5: Option<String>,
    #[serde(default)]
    git_commit: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct MyClangType {
    #[serde(rename = "qualType")]
    qual_type: String,
    #[serde(rename = "desugaredQualType")]
    desugared_qual_type: Option<String>,
}

impl MyClangType {
    fn typedef(&self) -> &str {
        self.desugared_qual_type
            .as_deref()
            .unwrap_or(&self.qual_type)
    }

    fn fill_array_size(&self, c_code: &mut String) -> Result<()> {
        let ty = Self::ignore_fn(self.typedef());
        let (off, end) = Self::ignore_fn_range(ty);
        let ty_without_fn = &ty[off..end];

        let array_re = regex::Regex::new(r"(\[\s*\d*\s*\]\s*)+$").unwrap();

        let c_code_match = if let Some(cap) = array_re.captures(c_code) {
            cap[0].to_string()
        } else {
            return Ok(());
        };

        let typedef_match = if let Some(cap) = array_re.captures(ty_without_fn) {
            cap[0].to_string()
        } else {
            return Err(Error::inval());
        };

        let typedef_count = typedef_match.chars().filter(|&c| c == '[').count();
        let c_code_count = c_code_match.chars().filter(|&c| c == '[').count();

        if c_code_count != typedef_count {
            return Err(Error::inval());
        }

        let cap = array_re.captures(c_code).unwrap();
        let (start, end) = (cap.get(0).unwrap().start(), cap.get(0).unwrap().end());
        c_code.replace_range(start..end, &typedef_match);

        Ok(())
    }

    fn is_const(&self) -> bool {
        Self::is_const_ty(self.typedef())
    }

    fn is_const_ty(ty: &str) -> bool {
        let re = regex::Regex::new(r"\bconst\b[^\*]*$").unwrap();
        re.is_match(Self::ignore_fn(ty))
    }

    fn ignore_fn(ty: &str) -> &str {
        let (off, end) = Self::ignore_fn_range(ty);
        &ty[off..end]
    }

    fn ignore_fn_range(ty: &str) -> (usize, usize) {
        let re = regex::Regex::new(r"^[^\(]*\(\s*[^\s]").unwrap();
        let mut off = 0;
        let mut end = ty.len();
        while let Some(m) = re.find(&ty[off..end]) {
            if ty.as_bytes()[off + m.len() - 1] != b'*' {
                break;
            }
            off += m.len();
            let mut cnt = 1;
            for n in off..end {
                match ty.as_bytes()[n] {
                    b'(' => cnt += 1,
                    b')' => {
                        cnt -= 1;
                        if cnt == 0 {
                            end = n;
                            break;
                        }
                    }
                    _ => continue,
                }
            }
        }
        (off, end)
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TypedefDecl {
    name: String,
    loc: SourceLocation,
    range: SourceRange,
    #[serde(rename = "type")]
    ty: MyClangType,
    #[serde(default, rename = "isImplicit")]
    is_implicit: bool,
    #[serde(default)]
    skip: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct EnumDecl {
    name: Option<String>,
    loc: SourceLocation,
    range: SourceRange,
    #[serde(default, rename = "completeDefinition")]
    is_definition: bool,
    #[serde(default, rename = "isImplicit")]
    is_implicit: bool,
    #[serde(default)]
    skip: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RecordDecl {
    name: Option<String>,
    loc: SourceLocation,
    range: SourceRange,
    #[serde(rename = "tagUsed")]
    tag_used: String,
    #[serde(default, rename = "completeDefinition")]
    is_definition: bool,
    #[serde(default, rename = "isImplicit")]
    is_implicit: bool,
    #[serde(default)]
    skip: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct VarDecl {
    name: String,
    loc: SourceLocation,
    range: SourceRange,
    #[serde(rename = "type")]
    ty: MyClangType,
    #[serde(rename = "storageClass")]
    storage_class: Option<String>,
    init: Option<String>,
    #[serde(default)]
    fake_init: bool,
    #[serde(default, rename = "isImplicit")]
    is_implicit: bool,
    #[serde(default)]
    git_commit: bool,
    global_name: Option<String>,
    #[serde(default)]
    skip: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FunctionDecl {
    name: String,
    loc: SourceLocation,
    range: SourceRange,
    #[serde(default, rename = "completeDefinition")]
    is_definition: bool,
    #[serde(default, rename = "isImplicit")]
    is_implicit: bool,
    #[serde(default)]
    inline: bool,
    #[serde(rename = "type")]
    ty: MyClangType,
    #[serde(default, rename = "storageClass")]
    storage_class: Option<String>,
    #[serde(default)]
    git_commit: bool,
    global_name: Option<String>,
    #[serde(default)]
    skip: bool,
}

pub(crate) struct File {
    node: Node,
    path: PathBuf,
    loaded_from_json: bool,
}

impl File {
    // root: $C2RUST_PROJECT_ROOT/.c2rust/c
    pub fn new(root: &Path, path: &Path) -> Result<Self> {
        let json_file = path.with_extension("json");

        if json_file.exists() {
            // 尝试从 JSON 文件加载
            if let Ok(node) = Self::load_from_json(&json_file) {
                return Ok(Self {
                    node,
                    path: path.to_path_buf(),
                    loaded_from_json: true,
                });
            }
            // 加载失败，继续从 C 文件解析
        }

        // 从 C 文件解析
        Self::with_c_file(root, path)
    }

    pub fn save_json(&self) -> Result<()> {
        Self::save_to(&self.node, &self.path)
    }

    fn save_to(node: &Node, path: &Path) -> Result<()> {
        let json_file = path.with_extension("json");
        let json = serde_json::to_string_pretty(node).map_err(|_| Error::nomem())?;
        fs::write(&json_file, json).map_err(|_| Error::last())
    }

    fn load_from_json(json_file: &Path) -> Result<Node> {
        let content =
            fs::read_to_string(json_file).log_err(&format!("read {}", json_file.display()))?;
        let node: Node =
            serde_json::from_str(&content).log_err(&format!("parse {}", json_file.display()))?;
        Ok(node)
    }

    fn with_c_file(root: &Path, path: &Path) -> Result<Self> {
        let mut node = Self::load_by_c_file(root, path)?;
        let Kind::TranslationUnitDecl(ref mut unit) = node.kind else {
            return Err(Error::inval());
        };
        unit.md5 = Some(Self::md5_file(path)?);
        Self::save_to(&node, path)?;
        Ok(Self {
            node,
            path: path.to_path_buf(),
            loaded_from_json: false,
        })
    }

    // root: $C2RUST_PROJECT_ROOT/.c2rust/c
    fn remove_static(root: &Path, path: &Path, mut node: Node) -> Result<Node> {
        let md5 = Self::md5_file(&path.with_extension("c2rust"))?;

        if !Self::rename_static_symbols(&mut node, &md5) {
            return Ok(node);
        }

        let new_c_file = path.with_extension("c2rust_without_static");
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
            if !child.kind.is_static() && !child.kind.is_inline() {
                continue;
            }
            let Some(name) = child.kind.name() else {
                continue;
            };
            child
                .kind
                .set_global_name(format!("_c2rust_private_{md5}_{name}"));
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

    fn load_by_c_file(root: &Path, path: &Path) -> Result<Node> {
        let Ok(rel_path) = path.strip_prefix(root) else {
            return Err(Error::inval());
        };
        let output = Command::new(get_clang())
            .current_dir(root)
            .arg("-xc")
            .arg("-Xclang")
            .arg("-ast-dump=json")
            .arg("-fsyntax-only")
            .arg(rel_path)
            .output()
            .log_err("clang -xc -Xclang -ast-dump=json -fsyntax-only")?;
        // clang解析gcc预处理文件是可能出错，但是忽略，直接查看输出的json文件是否可用.
        let mut err_msg = String::new();
        if !output.status.success() {
            err_msg = String::from_utf8_lossy(&output.stderr).to_string();
        }
        let json = String::from_utf8_lossy(&output.stdout);
        let mut node = serde_json::from_str(&json).log_err(&err_msg)?;
        let js = path.with_extension("json.tmp");
        if js.exists() {
            let _ = fs::write(path.with_extension("js2"), json.as_bytes());
        } else {
            let _ = fs::write(path.with_extension("js"), json.as_bytes());
        }
        Self::init_line_info(&mut node);
        Self::init_vars(&mut node);
        Self::remove_static(root, path, node)
    }

    fn md5_file(path: &Path) -> Result<String> {
        let content = fs::read_to_string(path).log_err(&format!("read {}", path.display()))?;
        let digest = md5::compute(content.as_bytes());
        Ok(format!("{:x}", digest))
    }

    // 同名 FunctionDecl：只要其中一个 inline=true，则同名全部 inline=true
    fn normalize_inline_flags(node: &mut Node) {
        use std::collections::HashMap;

        // name(String) -> any_inline
        let mut any_inline: HashMap<String, bool> = HashMap::new();

        // first pass: collect whether any inline=true exists for each name
        for child in &node.inner {
            let Kind::FunctionDecl(ref f) = child.kind else {
                continue;
            };

            // entry 默认 false；出现 true 就置 true（并保持 true）
            let e = any_inline.entry(f.name.clone()).or_insert(false);
            if f.inline {
                *e = true;
            }
        }

        // second pass: write back unified inline flag
        for child in &mut node.inner {
            let Kind::FunctionDecl(ref mut f) = child.kind else {
                continue;
            };

            if any_inline.get(&f.name) == Some(&true) {
                f.inline = true;
            }
        }
    }

    fn init_line_info(node: &mut Node) {
        if node.inner.is_empty() {
            return;
        }
        for n in 1..node.inner.len() {
            let src = node.inner[n - 1].kind.loc().cloned();
            let target = node.inner[n].kind.loc_mut();
            if let (Some(target), Some(src)) = (target, src) {
                init_loc(target, &src);
            }
        }

        // typedef struct Foo { } Foo, 就是前序节点被后续节点包含.
        for n in 0..node.inner.len() {
            if node.inner[n].kind.skip() {
                continue;
            }
            let Some(range1) = node.inner[n].kind.range() else {
                continue;
            };
            for m in (n + 1)..node.inner.len() {
                let Some(range2) = node.inner[m].kind.range() else {
                    continue;
                };
                if !range_include(range2, range1) {
                    continue;
                }
                for k in n..m {
                    node.inner[k].kind.set_skip();
                }
                break;
            }
        }

        // void foo(int, ...), 存在后续节点被前序节点包含
        for n in (0..node.inner.len()).rev() {
            if node.inner[n].kind.skip() {
                continue;
            }
            let Some(range1) = node.inner[n].kind.range() else {
                continue;
            };
            for m in (0..n).rev() {
                let Some(range2) = node.inner[m].kind.range() else {
                    continue;
                };
                if !range_include(range2, range1) {
                    continue;
                }
                for k in m + 1..n + 1 {
                    node.inner[k].kind.set_skip();
                }
                break;
            }
        }
        node.inner.retain(|node| !node.kind.skip());
        Self::normalize_inline_flags(node);
    }

    fn init_vars(node: &mut Node) {
        // 每个变量都必须有个init标志, 如果都没有，选择一个设置为fake_init标志.
        let mut inited_vars = HashMap::new();
        for node in &mut node.inner {
            let Kind::VarDecl(ref mut var) = node.kind else {
                continue;
            };
            if var.init.is_some() {
                inited_vars.insert(var.name.clone(), var);
            } else if inited_vars.contains_key(&var.name) {
                // 多余的，可以删除.
                var.is_implicit = true;
            } else {
                inited_vars.insert(var.name.clone(), var);
            }
        }
        for (_, var) in inited_vars {
            if var.storage_class.as_deref() != Some("extern") && var.init.is_none() {
                var.fake_init = true;
            }
        }
    }

    pub fn iter(&self) -> &[Node] {
        &self.node.inner
    }

    pub fn iter_mut(&mut self) -> &mut [Node] {
        &mut self.node.inner
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn loaded_from_json(&self) -> bool {
        self.loaded_from_json
    }

    // root: $C2RUST_PROJECT_ROOT/.c2rust/c
    pub fn export_c_code(&self, root: &Path) -> Result<String> {
        Self::generate_c_code(root, &self.node.inner, false, false)
    }

    pub fn export_header(&self, root: &Path) -> Result<String> {
        let header = Self::generate_c_code(root, &self.node.inner, true, false)?;
        // bindgen基于clang，clang需要定义，但是gcc的预处理没有，无条件添加，重复无影响
        let mut new_header = "typedef float _Float32;\n".to_string();
        new_header.push_str("typedef double _Float64;\n");
        new_header.push_str("typedef double _Float32x;\n");
        new_header.push_str("typedef long double _Float64x;\n");
        new_header.push_str("typedef __float128 _Float128;\n");
        new_header.push_str(&header);
        Ok(new_header)
    }

    // fix: 表示之前生成的代码出现了错误，需要调整生成策略.
    fn generate_c_code(root: &Path, nodes: &[Node], is_header: bool, fix: bool) -> Result<String> {
        let mut content = String::new();
        let mut last: Option<&Node> = None;
        for node in nodes {
            let end = node.kind.range();
            let code = if let Some(last) = last {
                last.kind.tail_code(root, end)?
            } else {
                read_code_between(root, None, end)?
            };
            content.push_str(&code);
            last = Some(node);

            // 当前不支持添加line_info信息
            // 原因是tail_code中可能包括上一个语句的attribute，也可能包括本语句的特殊前缀__extension，不包含在json文件中.
            // line_info的插入位置难以确定.
            let mut code = node.kind.c_code(root)?;
            if code.is_empty() {
                continue;
            }

            if is_header || node.kind.has_committed() {
                if let Kind::FunctionDecl(_) = node.kind {
                    if let Some(pos) = code.find('{') {
                        code.drain(pos..);
                        code.push(';');
                    }
                    ensure_extern_decl(&mut code)?;
                }
                // 全局变量可能是static int g_i32[] = { ... }
                // 数组大小只能从var.ty中提取.
                if let Kind::VarDecl(ref var) = node.kind {
                    if let Some(pos) = code.find('=') {
                        code.drain(pos..);
                    }
                    ensure_extern_decl(&mut code)?;
                    var.ty.fill_array_size(&mut code)?;
                }
            }
            if let Some(rename) = node.kind.rename_macro() {
                // 在哪里插入这个宏是一个问题，函数本身可能递归，函数前和后也可能存在一些特定的扩展属性以及注释信息.
                // 只有插入到文件头相对好一些，但存在潜在的问题，就是之前的某个函数中存在名字冲突这种场景，相对罕见.
                // 先尝试插入到函数头，如果出现了编译错误，则插入到文件头.
                if !fix {
                    content.push_str(&format!("\n{rename}\n"));
                } else {
                    content.insert_str(0, &format!("\n{rename}\n"));
                }
            }
            content.push_str(&code);
        }
        if let Some(last) = last {
            let code = last.kind.tail_code(root, None)?;
            content.push_str(&code);
        }
        remove_unused_attrs(&mut content);
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_static_function_node(name: &str) -> Node {
        Node {
            id: clang_ast::Id::NULL,
            kind: Kind::FunctionDecl(FunctionDecl {
                name: name.to_string(),
                loc: SourceLocation::default(),
                range: SourceRange::default(),
                is_definition: true,
                is_implicit: false,
                inline: false,
                ty: MyClangType {
                    qual_type: "void ()".to_string(),
                    desugared_qual_type: None,
                },
                storage_class: Some("static".to_string()),
                git_commit: false,
                global_name: None,
                skip: false,
            }),
            inner: vec![],
        }
    }

    fn make_non_static_function_node(name: &str) -> Node {
        Node {
            id: clang_ast::Id::NULL,
            kind: Kind::FunctionDecl(FunctionDecl {
                name: name.to_string(),
                loc: SourceLocation::default(),
                range: SourceRange::default(),
                is_definition: true,
                is_implicit: false,
                inline: false,
                ty: MyClangType {
                    qual_type: "void ()".to_string(),
                    desugared_qual_type: None,
                },
                storage_class: None,
                git_commit: false,
                global_name: None,
                skip: false,
            }),
            inner: vec![],
        }
    }

    fn make_static_var_node(name: &str) -> Node {
        Node {
            id: clang_ast::Id::NULL,
            kind: Kind::VarDecl(VarDecl {
                name: name.to_string(),
                loc: SourceLocation::default(),
                range: SourceRange::default(),
                ty: MyClangType {
                    qual_type: "int".to_string(),
                    desugared_qual_type: None,
                },
                storage_class: Some("static".to_string()),
                init: None,
                is_implicit: false,
                git_commit: false,
                global_name: None,
                skip: false,
                fake_init: false,
            }),
            inner: vec![],
        }
    }

    fn make_var(qual_type: &str) -> Node {
        Node {
            id: clang_ast::Id::NULL,
            kind: Kind::VarDecl(VarDecl {
                name: "test_var".to_string(),
                loc: SourceLocation::default(),
                range: SourceRange::default(),
                ty: MyClangType {
                    qual_type: qual_type.to_string(),
                    desugared_qual_type: None,
                },
                storage_class: None,
                init: None,
                is_implicit: false,
                git_commit: false,
                global_name: None,
                skip: false,
                fake_init: false,
            }),
            inner: vec![],
        }
    }

    #[test]
    fn test_is_const_var_top_level() {
        assert!(make_var("const int").kind.is_const_var());
        assert!(make_var("int const").kind.is_const_var());
        assert!(!make_var("int").kind.is_const_var());
    }

    #[test]
    fn test_is_const_var_pointer() {
        assert!(make_var("int * const").kind.is_const_var());
        assert!(!make_var("const int *").kind.is_const_var());
        assert!(!make_var("int const *").kind.is_const_var());
        assert!(!make_var("int *").kind.is_const_var());
    }

    #[test]
    fn test_is_const_var_no_match_keywords() {
        assert!(!make_var("constexpr int").kind.is_const_var());
        assert!(!make_var("auto const_cast<T>").kind.is_const_var());
        assert!(!make_var("myconst int").kind.is_const_var());
        assert!(!make_var("constify int").kind.is_const_var());
    }

    #[test]
    fn test_is_const_var_complex_types() {
        assert!(make_var("const struct Point").kind.is_const_var());
        assert!(make_var("struct Point * const").kind.is_const_var());
        assert!(!make_var("const struct Point *").kind.is_const_var());
        assert!(make_var("const enum Color").kind.is_const_var());
        assert!(make_var("enum Color * const").kind.is_const_var());
    }

    #[test]
    fn test_is_const_var_multi_level_pointer() {
        assert!(make_var("int ** const").kind.is_const_var());
        assert!(!make_var("int * const *").kind.is_const_var());
        assert!(!make_var("const int **").kind.is_const_var());
        assert!(make_var("int *** const").kind.is_const_var());
    }

    #[test]
    fn test_rename_static_symbols_no_static() {
        let mut node = Node {
            id: clang_ast::Id::NULL,
            kind: Kind::TranslationUnitDecl(TranslationUnitDecl {
                md5: None,
                git_commit: false,
            }),
            inner: vec![
                make_non_static_function_node("foo"),
                make_non_static_function_node("bar"),
            ],
        };

        let result = File::rename_static_symbols(&mut node, "abc123");
        assert!(!result);

        assert!(node.inner[0].kind.global_name().is_none());
        assert!(node.inner[1].kind.global_name().is_none());
    }

    #[test]
    fn test_rename_static_symbols_with_static() {
        let mut node = Node {
            id: clang_ast::Id::NULL,
            kind: Kind::TranslationUnitDecl(TranslationUnitDecl {
                md5: None,
                git_commit: false,
            }),
            inner: vec![
                make_static_function_node("private_func"),
                make_non_static_function_node("public_func"),
            ],
        };

        let result = File::rename_static_symbols(&mut node, "abc123");
        assert!(result);

        assert_eq!(
            node.inner[0].kind.global_name(),
            Some("_c2rust_private_abc123_private_func")
        );
        assert!(node.inner[1].kind.global_name().is_none());
    }

    #[test]
    fn test_rename_static_symbols_mixed() {
        let mut node = Node {
            id: clang_ast::Id::NULL,
            kind: Kind::TranslationUnitDecl(TranslationUnitDecl {
                md5: None,
                git_commit: false,
            }),
            inner: vec![
                make_static_function_node("static_fn"),
                make_static_var_node("static_var"),
                make_non_static_function_node("public_fn"),
            ],
        };

        let result = File::rename_static_symbols(&mut node, "xyz789");
        assert!(result);

        assert_eq!(
            node.inner[0].kind.global_name(),
            Some("_c2rust_private_xyz789_static_fn")
        );
        assert_eq!(
            node.inner[1].kind.global_name(),
            Some("_c2rust_private_xyz789_static_var")
        );
        assert!(node.inner[2].kind.global_name().is_none());
    }

    #[test]
    fn test_ignore_fn() {
        assert_eq!(
            MyClangType::ignore_fn(" const int (* foo (int, int*) )(int, int)"),
            " foo (int, int*) "
        );
        assert_eq!(
            MyClangType::ignore_fn(" const int (*(*foo (int, int*))(const char*) )(int, int)"),
            "foo (int, int*)"
        );
        assert_eq!(
            MyClangType::ignore_fn("int foo(int, int)"),
            "int foo(int, int)"
        );
    }

    #[test]
    fn test_is_const() {
        assert!(MyClangType::is_const_ty("const int"));
        assert!(MyClangType::is_const_ty("int const"));
        assert!(MyClangType::is_const_ty("volatile const int"));
        assert!(MyClangType::is_const_ty("volatile const int [3]"));
        assert!(MyClangType::is_const_ty("volatile const int [4][3]"));
        assert!(!MyClangType::is_const_ty("volatile const int* [4][3]"));
        assert!(!MyClangType::is_const_ty("int const* "));
        assert!(!MyClangType::is_const_ty("const int*"));
        assert!(!MyClangType::is_const_ty("const int(*)(const int)"));
    }

    #[allow(non_local_definitions)]
    impl MyClangType {
        fn new(s: &str) -> Self {
            Self {
                qual_type: s.to_string(),
                desugared_qual_type: None,
            }
        }
    }

    #[test]
    fn test_fill_array_size_all_empty() {
        let ty = MyClangType::new("const int name [5] [10]");
        let mut code = "const int name [] []".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name [5] [10]");
    }

    #[test]
    fn test_fill_array_size_partial_empty() {
        let ty = MyClangType::new("const int name [5] [10]");
        let mut code = "const int name [] [10]".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name [5] [10]");
    }

    #[test]
    fn test_fill_array_size_with_spaces() {
        let ty = MyClangType::new("const int name [ 5 ] [ 10 ]");
        let mut code = "const int name [ ] [ ]".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name [ 5 ] [ 10 ]");
    }

    #[test]
    fn test_fill_array_size_multiple_spaces_between() {
        let ty = MyClangType::new("const int name [5]  [10]");
        let mut code = "const int name []  []".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name [5]  [10]");
    }

    #[test]
    fn test_fill_array_size_no_array() {
        let ty = MyClangType::new("const int name");
        let mut code = "const int name".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name");
    }

    #[test]
    fn test_fill_array_size_code_no_brackets() {
        let ty = MyClangType::new("const int name [5]");
        let mut code = "const int name".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name");
    }

    #[test]
    fn test_fill_array_size_count_mismatch() {
        let ty = MyClangType::new("const int name [5] [10]");
        let mut code = "const int name []".to_string();
        let result = ty.fill_array_size(&mut code);
        assert!(result.is_err());
    }

    #[test]
    fn test_fill_array_size_typedef_no_array_code_has() {
        let ty = MyClangType::new("const int name");
        let mut code = "const int name []".to_string();
        let result = ty.fill_array_size(&mut code);
        assert!(result.is_err());
    }

    #[test]
    fn test_fill_array_size_many_spaces() {
        let ty = MyClangType::new("const int name   [   5   ]   [   10   ]");
        let mut code = "const int name   [   ]   [   ]".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name   [   5   ]   [   10   ]");
    }

    #[test]
    fn test_fill_array_size_only_empty() {
        let ty = MyClangType::new("const int name []");
        let mut code = "const int name []".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name []");
    }

    #[test]
    fn test_fill_array_size_typedef_leading_space() {
        let ty = MyClangType::new("const int name [5]");
        let mut code = "const int name [5]".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name [5]");
    }

    #[test]
    fn test_fill_array_size_partial_lost() {
        let ty = MyClangType::new("const int name [3] [7] [15]");
        let mut code = "const int name [] [7] [15]".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name [3] [7] [15]");
    }

    #[test]
    fn test_fill_array_size_all_correct() {
        let ty = MyClangType::new("const int name [5] [10]");
        let mut code = "const int name [5] [10]".to_string();
        ty.fill_array_size(&mut code).unwrap();
        assert_eq!(code, "const int name [5] [10]");
    }
}
