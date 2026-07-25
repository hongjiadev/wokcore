use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use proc_macro2::{TokenStream, TokenTree};
use syn::{
    Attribute, Expr, ForeignItem, ImplItem, Item, ItemImpl, ItemMod, ItemStruct, ItemUse, Local,
    Macro, Meta, PathArguments, StmtMacro, TraitItem, Type, UseTree, Visibility,
    ext::IdentExt,
    parse::Parser,
    punctuated::Punctuated,
    visit::{self, Visit},
};

fn detect_forbidden_native_access(
    source: &str,
    scan_entire_file: bool,
) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    if scan_entire_file {
        let mut visitor = ForbiddenNativeAccess::default();
        visitor.visit_file(&syntax);
        Ok(visitor.violations)
    } else {
        let mut visitor = TestItemVisitor::default();
        visitor.visit_file(&syntax);
        Ok(visitor.violations)
    }
}

fn scan_crate_test_sources(manifest_dir: &Path) -> io::Result<Vec<String>> {
    let crate_root = canonical_path(manifest_dir)?;
    let src_dir = manifest_dir.join("src");
    let tests_dir = manifest_dir.join("tests");
    let mut source_files = Vec::new();
    let mut test_files = Vec::new();
    collect_rust_files(&src_dir, &mut source_files)?;
    collect_rust_files(&tests_dir, &mut test_files)?;

    let mut violations = Vec::new();
    let mut scanned_entirely = HashSet::new();
    for path in &source_files {
        let source = fs::read_to_string(path)?;
        let syntax = parse_source(path, &source)?;
        let relative = path.strip_prefix(&src_dir).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to relativize {}: {error}", path.display()),
            )
        })?;
        append_violations(
            path,
            detect_forbidden_production_access(relative, &syntax),
            &mut violations,
        );
        scanned_entirely.insert(canonical_path(path)?);
    }
    for path in &test_files {
        let source = fs::read_to_string(path)?;
        let detected = detect_forbidden_native_access(&source, true)
            .map_err(|error| parse_error(path, error))?;
        append_violations(path, detected, &mut violations);
        scanned_entirely.insert(canonical_path(path)?);
    }

    let mut module_files = source_files
        .into_iter()
        .filter(|path| is_source_crate_root(path, &src_dir))
        .collect::<Vec<_>>();
    module_files.extend(test_files);
    {
        let mut scan = SourceGraphScan {
            crate_root: &crate_root,
            scanned_entirely: &scanned_entirely,
            visited: HashSet::new(),
            active_sources: Vec::new(),
            included_modes: HashMap::new(),
            violations: &mut violations,
        };
        for path in module_files {
            let path_is_test = path.starts_with(&tests_dir);
            scan_test_modules(&path, path_is_test, &mut scan)?;
        }
    }
    Ok(violations)
}

fn is_source_crate_root(path: &Path, src_dir: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "lib.rs" | "main.rs"))
        || path.starts_with(src_dir.join("bin"))
}

fn parse_source(path: &Path, source: &str) -> io::Result<syn::File> {
    syn::parse_file(source).map_err(|error| parse_error(path, error))
}

fn parse_error(path: &Path, error: syn::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("failed to parse {}: {error}", path.display()),
    )
}

fn append_violations(path: &Path, detected: Vec<String>, violations: &mut Vec<String>) {
    violations.extend(
        detected
            .into_iter()
            .map(|violation| format!("{}: {violation}", path.display())),
    );
}

fn canonical_path(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to resolve {}: {error}", path.display()),
        )
    })
}

fn collect_rust_files(directory: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rust_files(&path, paths)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
    Ok(())
}

fn detect_forbidden_production_access(relative: &Path, syntax: &syn::File) -> Vec<String> {
    let relative = relative.to_string_lossy().replace('\\', "/");
    let allowed_reexport = match relative.as_str() {
        "lib.rs" => Some(&["secrets", "NativeSecretStore"][..]),
        "secrets/mod.rs" => Some(&["native", "NativeSecretStore"][..]),
        _ => None,
    };
    if relative == "secrets/native.rs" {
        return detect_native_boundary_violations(syntax);
    }

    let mut violations = Vec::new();
    for item in &syntax.items {
        let allow_native_reexport = match (allowed_reexport, item) {
            (Some(expected), Item::Use(item_use)) => is_exact_public_reexport(item_use, expected),
            _ => false,
        };
        let mut visitor = ForbiddenNativeAccess::with_native_store_allowed(allow_native_reexport);
        visitor.visit_item(item);
        violations.extend(visitor.violations);
    }
    violations
}

fn detect_native_boundary_violations(syntax: &syn::File) -> Vec<String> {
    let mut violations = Vec::new();
    for item in &syntax.items {
        match item {
            Item::Use(item_use) if is_exact_keyring_import(item_use) => {}
            Item::Struct(item_struct) if is_exact_native_store_struct(item_struct) => {}
            Item::Impl(item_impl) if is_native_store_inherent_impl(item_impl) => {
                let mut visitor = ForbiddenNativeAccess::default();
                for impl_item in &item_impl.items {
                    visitor.visit_impl_item(impl_item);
                }
                violations.extend(visitor.violations);
            }
            Item::Impl(item_impl) if is_native_store_trait_impl(item_impl) => {}
            _ => {
                let mut visitor = ForbiddenNativeAccess::default();
                visitor.visit_item(item);
                violations.extend(visitor.violations);
            }
        }
    }
    violations
}

fn is_exact_keyring_import(item: &ItemUse) -> bool {
    if !matches!(item.vis, Visibility::Inherited) || item.leading_colon.is_some() {
        return false;
    }
    let mut leaves = Vec::new();
    flatten_use_tree(&item.tree, &mut Vec::new(), &mut leaves);
    leaves.sort();
    leaves
        == [
            (
                vec!["keyring".to_owned(), "Entry".to_owned()],
                None::<String>,
            ),
            (
                vec!["keyring".to_owned(), "Error".to_owned()],
                Some("KeyringError".to_owned()),
            ),
        ]
}

fn is_exact_public_reexport(item: &ItemUse, expected: &[&str]) -> bool {
    if !matches!(item.vis, Visibility::Public(_)) || item.leading_colon.is_some() {
        return false;
    }
    let mut leaves = Vec::new();
    flatten_use_tree(&item.tree, &mut Vec::new(), &mut leaves);
    let native_leaves = leaves
        .iter()
        .filter(|(path, _)| path.last().is_some_and(|name| name == "NativeSecretStore"))
        .collect::<Vec<_>>();
    native_leaves.len() == 1
        && native_leaves[0]
            .0
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
        && native_leaves[0].1.is_none()
}

fn flatten_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    leaves: &mut Vec<(Vec<String>, Option<String>)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(unraw_identifier(&path.ident).to_string());
            flatten_use_tree(&path.tree, prefix, leaves);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let mut path = prefix.clone();
            path.push(unraw_identifier(&name.ident).to_string());
            leaves.push((path, None));
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.clone();
            path.push(unraw_identifier(&rename.ident).to_string());
            leaves.push((path, Some(unraw_identifier(&rename.rename).to_string())));
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                flatten_use_tree(tree, prefix, leaves);
            }
        }
        UseTree::Glob(_) => {
            let mut path = prefix.clone();
            path.push("*".to_owned());
            leaves.push((path, None));
        }
    }
}

fn unraw_identifier(identifier: &syn::Ident) -> syn::Ident {
    identifier.unraw()
}

fn is_exact_native_store_struct(item: &ItemStruct) -> bool {
    item.ident == "NativeSecretStore"
        && matches!(item.vis, Visibility::Public(_))
        && matches!(item.fields, syn::Fields::Unit)
        && item.generics.params.is_empty()
        && item.generics.where_clause.is_none()
}

fn is_native_store_inherent_impl(item: &ItemImpl) -> bool {
    is_plain_impl(item)
        && item.trait_.is_none()
        && type_is_named(&item.self_ty, "NativeSecretStore")
}

fn is_native_store_trait_impl(item: &ItemImpl) -> bool {
    is_plain_impl(item)
        && item.trait_.as_ref().is_some_and(|(_, path, _)| {
            path.leading_colon.is_none()
                && path.segments.len() == 1
                && path.segments[0].ident == "SecretStore"
                && matches!(path.segments[0].arguments, PathArguments::None)
        })
        && type_is_named(&item.self_ty, "NativeSecretStore")
}

fn is_plain_impl(item: &ItemImpl) -> bool {
    item.defaultness.is_none()
        && item.unsafety.is_none()
        && item.generics.params.is_empty()
        && item.generics.where_clause.is_none()
}

fn type_is_named(ty: &Type, expected: &str) -> bool {
    matches!(
        ty,
        Type::Path(path)
            if path.qself.is_none()
                && path.path.segments.len() == 1
                && path.path.segments[0].ident == expected
                && matches!(path.path.segments[0].arguments, PathArguments::None)
    )
}

struct SourceGraphScan<'scan> {
    crate_root: &'scan Path,
    scanned_entirely: &'scan HashSet<PathBuf>,
    visited: HashSet<(PathBuf, bool)>,
    active_sources: Vec<PathBuf>,
    included_modes: HashMap<PathBuf, bool>,
    violations: &'scan mut Vec<String>,
}

fn scan_test_modules(
    path: &Path,
    test_mode: bool,
    scan: &mut SourceGraphScan<'_>,
) -> io::Result<()> {
    let canonical = canonical_path(path)?;
    if !scan.visited.insert((canonical.clone(), test_mode)) {
        return Ok(());
    }

    scan.active_sources.push(canonical.clone());
    let result = (|| {
        let source = fs::read_to_string(&canonical)?;
        let syntax = parse_source(&canonical, &source)?;
        if !scan.scanned_entirely.contains(&canonical) {
            let detected = if test_mode {
                detect_forbidden_native_access(&source, true)
                    .map_err(|error| parse_error(&canonical, error))?
            } else {
                detect_forbidden_production_access(Path::new("__included_source.rs"), &syntax)
            };
            append_violations(&canonical, detected, scan.violations);
        }

        scan_include_macros(&syntax, &canonical, test_mode, scan)?;

        let module_dir = module_directory(&canonical);
        scan_item_modules(&syntax.items, &canonical, &module_dir, test_mode, scan)
    })();
    scan.active_sources.pop();
    result
}

fn scan_include_macros(
    syntax: &syn::File,
    source_path: &Path,
    inherited_test_mode: bool,
    scan: &mut SourceGraphScan<'_>,
) -> io::Result<()> {
    let mut visitor = IncludeVisitor::new(inherited_test_mode);
    visitor.visit_file(syntax);
    if let Some(error) = visitor.errors.into_iter().next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid include! in {}: {error}", source_path.display()),
        ));
    }

    for directive in visitor.directives {
        let parent = source_path.parent().unwrap_or_else(|| Path::new(""));
        let candidate = parent.join(&directive.path);
        let included = candidate.canonicalize().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "failed to resolve include! {} declared in {}: {error}",
                    directive.path.display(),
                    source_path.display()
                ),
            )
        })?;
        if !included.starts_with(scan.crate_root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "include! {} declared in {} resolves outside crate root {}",
                    directive.path.display(),
                    source_path.display(),
                    scan.crate_root.display()
                ),
            ));
        }
        if !included
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "rs" | "inc"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "include! {} declared in {} must reference a .rs or .inc file",
                    directive.path.display(),
                    source_path.display()
                ),
            ));
        }
        if scan.active_sources.contains(&included) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("include cycle reaches {}", included.display()),
            ));
        }
        if scan
            .included_modes
            .insert(included.clone(), directive.test_mode)
            .is_some_and(|existing| existing != directive.test_mode)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "included source {} is reachable in both production and test modes",
                    included.display()
                ),
            ));
        }

        scan_test_modules(&included, directive.test_mode, scan)?;
    }
    Ok(())
}

struct IncludeDirective {
    path: PathBuf,
    test_mode: bool,
}

struct IncludeVisitor {
    directives: Vec<IncludeDirective>,
    errors: Vec<String>,
    test_mode: bool,
}

impl IncludeVisitor {
    fn new(test_mode: bool) -> Self {
        Self {
            directives: Vec::new(),
            errors: Vec::new(),
            test_mode,
        }
    }

    fn append(&mut self, nested: IncludeVisitor) {
        self.directives.extend(nested.directives);
        self.errors.extend(nested.errors);
    }
}

impl<'syntax> Visit<'syntax> for IncludeVisitor {
    fn visit_item_use(&mut self, item: &'syntax ItemUse) {
        let mut leaves = Vec::new();
        flatten_use_tree(&item.tree, &mut Vec::new(), &mut leaves);
        if leaves
            .iter()
            .any(|(path, _)| path.last().is_some_and(|segment| segment == "include"))
        {
            self.errors
                .push("include import is not supported".to_owned());
        }
        visit::visit_item_use(self, item);
    }

    fn visit_item(&mut self, item: &'syntax Item) {
        if !self.test_mode && item_attributes(item).iter().any(attribute_marks_test_code) {
            let mut nested = IncludeVisitor::new(true);
            visit::visit_item(&mut nested, item);
            self.append(nested);
        } else {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'syntax ImplItem) {
        if !self.test_mode
            && impl_item_attributes(item)
                .iter()
                .any(attribute_marks_test_code)
        {
            let mut nested = IncludeVisitor::new(true);
            visit::visit_impl_item(&mut nested, item);
            self.append(nested);
        } else {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'syntax TraitItem) {
        if !self.test_mode
            && trait_item_attributes(item)
                .iter()
                .any(attribute_marks_test_code)
        {
            let mut nested = IncludeVisitor::new(true);
            visit::visit_trait_item(&mut nested, item);
            self.append(nested);
        } else {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_foreign_item(&mut self, item: &'syntax ForeignItem) {
        if !self.test_mode
            && foreign_item_attributes(item)
                .iter()
                .any(attribute_marks_test_code)
        {
            let mut nested = IncludeVisitor::new(true);
            visit::visit_foreign_item(&mut nested, item);
            self.append(nested);
        } else {
            visit::visit_foreign_item(self, item);
        }
    }

    fn visit_local(&mut self, local: &'syntax Local) {
        if !self.test_mode && local.attrs.iter().any(attribute_marks_test_code) {
            let mut nested = IncludeVisitor::new(true);
            visit::visit_local(&mut nested, local);
            self.append(nested);
        } else {
            visit::visit_local(self, local);
        }
    }

    fn visit_expr(&mut self, expression: &'syntax Expr) {
        if !self.test_mode
            && expression_attributes(expression)
                .iter()
                .any(attribute_marks_test_code)
        {
            let mut nested = IncludeVisitor::new(true);
            visit::visit_expr(&mut nested, expression);
            self.append(nested);
        } else {
            visit::visit_expr(self, expression);
        }
    }

    fn visit_stmt_macro(&mut self, statement: &'syntax StmtMacro) {
        if !self.test_mode && statement.attrs.iter().any(attribute_marks_test_code) {
            let mut nested = IncludeVisitor::new(true);
            visit::visit_stmt_macro(&mut nested, statement);
            self.append(nested);
        } else {
            visit::visit_stmt_macro(self, statement);
        }
    }

    fn visit_macro(&mut self, item: &'syntax Macro) {
        if item.path.is_ident("macro_rules")
            && token_stream_contains_include_invocation(&item.tokens)
        {
            self.errors
                .push("macro_rules! may not generate an include! invocation".to_owned());
        } else if item
            .path
            .get_ident()
            .is_some_and(|identifier| unraw_identifier(identifier) == "include")
        {
            match syn::parse2::<syn::LitStr>(item.tokens.clone()) {
                Ok(path) => self.directives.push(IncludeDirective {
                    path: PathBuf::from(path.value()),
                    test_mode: self.test_mode,
                }),
                Err(_) => self
                    .errors
                    .push("include! must contain a single string literal".to_owned()),
            }
        } else if item
            .path
            .segments
            .last()
            .is_some_and(|segment| unraw_identifier(&segment.ident) == "include")
        {
            self.errors
                .push("qualified include! macros are not supported".to_owned());
        }
        visit::visit_macro(self, item);
    }
}

fn token_stream_contains_include_invocation(tokens: &TokenStream) -> bool {
    let tokens = tokens.clone().into_iter().collect::<Vec<_>>();
    tokens.iter().enumerate().any(|(index, token)| match token {
        TokenTree::Ident(identifier) if unraw_identifier(identifier) == "include" => matches!(
            tokens.get(index + 1),
            Some(TokenTree::Punct(punctuation)) if punctuation.as_char() == '!'
        ),
        TokenTree::Group(group) => token_stream_contains_include_invocation(&group.stream()),
        TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => false,
    })
}

fn scan_item_modules(
    items: &[Item],
    source_path: &Path,
    module_dir: &Path,
    inherited_test_mode: bool,
    scan: &mut SourceGraphScan<'_>,
) -> io::Result<()> {
    for item in items {
        let Item::Mod(item_mod) = item else {
            continue;
        };
        let module_test_mode =
            inherited_test_mode || item_mod.attrs.iter().any(attribute_gates_test_code);

        if let Some((_, items)) = &item_mod.content {
            let child_module_dir = module_dir.join(item_mod.ident.to_string());
            scan_item_modules(
                items,
                source_path,
                &child_module_dir,
                module_test_mode,
                scan,
            )?;
        } else if module_test_mode {
            let module_path = resolve_external_module(item_mod, source_path, module_dir, true)?;
            scan_test_modules(&module_path, true, scan)?;
        } else {
            let module_path = resolve_external_module(item_mod, source_path, module_dir, false)?;
            scan_test_modules(&module_path, false, scan)?;

            if cfg_attr_test_path_attribute(item_mod)?.is_some() {
                let test_module_path =
                    resolve_external_module(item_mod, source_path, module_dir, true)?;
                scan_test_modules(&test_module_path, true, scan)?;
            }
        }
    }
    Ok(())
}

fn module_directory(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    match path.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => path
            .file_stem()
            .map_or_else(|| parent.to_path_buf(), |stem| parent.join(stem)),
    }
}

fn resolve_external_module(
    item: &ItemMod,
    source_path: &Path,
    module_dir: &Path,
    test_mode: bool,
) -> io::Result<PathBuf> {
    let direct_path = module_path_attribute(item)?;
    let test_path = test_mode
        .then(|| cfg_attr_test_path_attribute(item))
        .transpose()?
        .flatten();
    let configured_path = match (direct_path, test_path) {
        (Some(_), Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "module {} declared in {} has ambiguous direct and test path attributes",
                    item.ident,
                    source_path.display()
                ),
            ));
        }
        (direct, test) => test.or(direct),
    };
    if let Some(path) = configured_path {
        let parent = source_path.parent().unwrap_or_else(|| Path::new(""));
        let candidate = parent.join(path);
        if candidate.is_file() {
            return Ok(candidate);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "test module {} declared in {} resolves to missing path {}",
                item.ident,
                source_path.display(),
                candidate.display()
            ),
        ));
    }

    let name = item.ident.to_string();
    let candidates = [
        module_dir.join(format!("{name}.rs")),
        module_dir.join(&name).join("mod.rs"),
    ];
    let existing = candidates
        .iter()
        .filter(|candidate| candidate.is_file())
        .collect::<Vec<_>>();
    match existing.as_slice() {
        [path] => Ok((*path).to_path_buf()),
        [] => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "test module {} declared in {} could not be resolved",
                item.ident,
                source_path.display()
            ),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "test module {} declared in {} is ambiguous: {} and {}",
                item.ident,
                source_path.display(),
                candidates[0].display(),
                candidates[1].display()
            ),
        )),
    }
}

fn module_path_attribute(item: &ItemMod) -> io::Result<Option<PathBuf>> {
    let mut result = None;
    for attribute in item
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("path"))
    {
        let Meta::NameValue(name_value) = &attribute.meta else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("module {} has an invalid path attribute", item.ident),
            ));
        };
        let Expr::Lit(expression) = &name_value.value else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("module {} has a non-literal path attribute", item.ident),
            ));
        };
        let syn::Lit::Str(path) = &expression.lit else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("module {} has a non-string path attribute", item.ident),
            ));
        };
        if result.replace(PathBuf::from(path.value())).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("module {} has multiple path attributes", item.ident),
            ));
        }
    }
    Ok(result)
}

fn cfg_attr_test_path_attribute(item: &ItemMod) -> io::Result<Option<PathBuf>> {
    let mut result = None;
    for attribute in item
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("cfg_attr"))
    {
        let Meta::List(list) = &attribute.meta else {
            return Err(invalid_cfg_attr_path(item, "is not a list"));
        };
        let arguments = parse_meta_arguments(list)
            .map_err(|error| invalid_cfg_attr_path(item, &error.to_string()))?;
        let Some(predicate) = arguments.first() else {
            return Err(invalid_cfg_attr_path(item, "has no predicate"));
        };
        if !matches!(
            cfg_relation(predicate),
            TestCfgRelation::Positive | TestCfgRelation::Unknown
        ) {
            continue;
        }
        for meta in arguments.iter().skip(1) {
            if meta.path().is_ident("path") {
                let Meta::NameValue(name_value) = meta else {
                    return Err(invalid_cfg_attr_path(
                        item,
                        "contains a non-value path attribute",
                    ));
                };
                let Expr::Lit(expression) = &name_value.value else {
                    return Err(invalid_cfg_attr_path(
                        item,
                        "contains a non-literal path value",
                    ));
                };
                let syn::Lit::Str(path) = &expression.lit else {
                    return Err(invalid_cfg_attr_path(
                        item,
                        "contains a non-string path value",
                    ));
                };
                if result.replace(PathBuf::from(path.value())).is_some() {
                    return Err(invalid_cfg_attr_path(
                        item,
                        "contains multiple test path attributes",
                    ));
                }
            } else if meta.path().is_ident("cfg_attr") {
                return Err(invalid_cfg_attr_path(
                    item,
                    "contains a nested cfg_attr that may select a path",
                ));
            }
        }
    }
    Ok(result)
}

fn invalid_cfg_attr_path(item: &ItemMod, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "module {} has an unsupported cfg_attr path shape: {reason}",
            item.ident
        ),
    )
}

#[derive(Default)]
struct ForbiddenNativeAccess {
    violations: Vec<String>,
    allow_native_store: bool,
}

impl ForbiddenNativeAccess {
    fn with_native_store_allowed(allow_native_store: bool) -> Self {
        Self {
            violations: Vec::new(),
            allow_native_store,
        }
    }

    fn check_identifier(&mut self, identifier: &syn::Ident) {
        let identifier = identifier.to_string();
        if self.allow_native_store && identifier == "NativeSecretStore" {
            return;
        }
        if matches!(
            identifier.as_str(),
            "Entry" | "NativeSecretStore" | "set_password" | "get_password" | "delete_credential"
        ) {
            self.violations.push(identifier);
        }
    }

    fn check_tokens(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Ident(identifier) => self.check_identifier(&identifier),
                TokenTree::Group(group) => self.check_tokens(group.stream()),
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
        }
    }
}

impl<'syntax> Visit<'syntax> for ForbiddenNativeAccess {
    fn visit_ident(&mut self, identifier: &'syntax syn::Ident) {
        self.check_identifier(identifier);
    }

    fn visit_macro(&mut self, item: &'syntax Macro) {
        visit::visit_macro(self, item);
        self.check_tokens(item.tokens.clone());
    }
}

#[derive(Default)]
struct TestItemVisitor {
    violations: Vec<String>,
}

impl<'syntax> Visit<'syntax> for TestItemVisitor {
    fn visit_item(&mut self, item: &'syntax Item) {
        if item_attributes(item).iter().any(attribute_marks_test_code) {
            let mut visitor = ForbiddenNativeAccess::default();
            visitor.visit_item(item);
            self.violations.extend(visitor.violations);
        } else {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'syntax ImplItem) {
        if impl_item_attributes(item)
            .iter()
            .any(attribute_marks_test_code)
        {
            let mut visitor = ForbiddenNativeAccess::default();
            visitor.visit_impl_item(item);
            self.violations.extend(visitor.violations);
        } else {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'syntax TraitItem) {
        if trait_item_attributes(item)
            .iter()
            .any(attribute_marks_test_code)
        {
            let mut visitor = ForbiddenNativeAccess::default();
            visitor.visit_trait_item(item);
            self.violations.extend(visitor.violations);
        } else {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_foreign_item(&mut self, item: &'syntax ForeignItem) {
        if foreign_item_attributes(item)
            .iter()
            .any(attribute_marks_test_code)
        {
            let mut visitor = ForbiddenNativeAccess::default();
            visitor.visit_foreign_item(item);
            self.violations.extend(visitor.violations);
        } else {
            visit::visit_foreign_item(self, item);
        }
    }

    fn visit_local(&mut self, local: &'syntax Local) {
        if local.attrs.iter().any(attribute_marks_test_code) {
            let mut visitor = ForbiddenNativeAccess::default();
            visitor.visit_local(local);
            self.violations.extend(visitor.violations);
        } else {
            visit::visit_local(self, local);
        }
    }

    fn visit_expr(&mut self, expression: &'syntax Expr) {
        if expression_attributes(expression)
            .iter()
            .any(attribute_marks_test_code)
        {
            let mut visitor = ForbiddenNativeAccess::default();
            visitor.visit_expr(expression);
            self.violations.extend(visitor.violations);
        } else {
            visit::visit_expr(self, expression);
        }
    }

    fn visit_stmt_macro(&mut self, statement: &'syntax StmtMacro) {
        if statement.attrs.iter().any(attribute_marks_test_code) {
            let mut visitor = ForbiddenNativeAccess::default();
            visitor.visit_stmt_macro(statement);
            self.violations.extend(visitor.violations);
        } else {
            visit::visit_stmt_macro(self, statement);
        }
    }
}

fn item_attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

fn impl_item_attributes(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(item) => &item.attrs,
        ImplItem::Fn(item) => &item.attrs,
        ImplItem::Macro(item) => &item.attrs,
        ImplItem::Type(item) => &item.attrs,
        _ => &[],
    }
}

fn trait_item_attributes(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(item) => &item.attrs,
        TraitItem::Fn(item) => &item.attrs,
        TraitItem::Macro(item) => &item.attrs,
        TraitItem::Type(item) => &item.attrs,
        _ => &[],
    }
}

fn foreign_item_attributes(item: &ForeignItem) -> &[Attribute] {
    match item {
        ForeignItem::Fn(item) => &item.attrs,
        ForeignItem::Macro(item) => &item.attrs,
        ForeignItem::Static(item) => &item.attrs,
        ForeignItem::Type(item) => &item.attrs,
        _ => &[],
    }
}

fn expression_attributes(expression: &Expr) -> &[Attribute] {
    match expression {
        Expr::Array(expression) => &expression.attrs,
        Expr::Assign(expression) => &expression.attrs,
        Expr::Async(expression) => &expression.attrs,
        Expr::Await(expression) => &expression.attrs,
        Expr::Binary(expression) => &expression.attrs,
        Expr::Block(expression) => &expression.attrs,
        Expr::Break(expression) => &expression.attrs,
        Expr::Call(expression) => &expression.attrs,
        Expr::Cast(expression) => &expression.attrs,
        Expr::Closure(expression) => &expression.attrs,
        Expr::Const(expression) => &expression.attrs,
        Expr::Continue(expression) => &expression.attrs,
        Expr::Field(expression) => &expression.attrs,
        Expr::ForLoop(expression) => &expression.attrs,
        Expr::Group(expression) => &expression.attrs,
        Expr::If(expression) => &expression.attrs,
        Expr::Index(expression) => &expression.attrs,
        Expr::Infer(expression) => &expression.attrs,
        Expr::Let(expression) => &expression.attrs,
        Expr::Lit(expression) => &expression.attrs,
        Expr::Loop(expression) => &expression.attrs,
        Expr::Macro(expression) => &expression.attrs,
        Expr::Match(expression) => &expression.attrs,
        Expr::MethodCall(expression) => &expression.attrs,
        Expr::Paren(expression) => &expression.attrs,
        Expr::Path(expression) => &expression.attrs,
        Expr::Range(expression) => &expression.attrs,
        Expr::RawAddr(expression) => &expression.attrs,
        Expr::Reference(expression) => &expression.attrs,
        Expr::Repeat(expression) => &expression.attrs,
        Expr::Return(expression) => &expression.attrs,
        Expr::Struct(expression) => &expression.attrs,
        Expr::Try(expression) => &expression.attrs,
        Expr::TryBlock(expression) => &expression.attrs,
        Expr::Tuple(expression) => &expression.attrs,
        Expr::Unary(expression) => &expression.attrs,
        Expr::Unsafe(expression) => &expression.attrs,
        Expr::While(expression) => &expression.attrs,
        Expr::Yield(expression) => &expression.attrs,
        _ => &[],
    }
}

fn attribute_marks_test_code(attribute: &Attribute) -> bool {
    if attribute_gates_test_code(attribute) {
        return true;
    }
    match &attribute.meta {
        Meta::List(list) if list.path.is_ident("cfg_attr") => {
            matches!(
                parse_cfg_attr_predicate(list),
                TestCfgRelation::Positive | TestCfgRelation::Unknown
            )
        }
        _ => false,
    }
}

fn attribute_gates_test_code(attribute: &Attribute) -> bool {
    if attribute
        .path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "test")
    {
        return true;
    }
    match &attribute.meta {
        Meta::List(list) if list.path.is_ident("cfg") => {
            matches!(
                parse_cfg_predicate(list),
                TestCfgRelation::Positive | TestCfgRelation::Unknown
            )
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestCfgRelation {
    Positive,
    Negative,
    Unrelated,
    Unknown,
}

fn parse_cfg_predicate(list: &syn::MetaList) -> TestCfgRelation {
    match parse_meta_arguments(list) {
        Ok(arguments) if arguments.len() == 1 => cfg_relation(&arguments[0]),
        _ => TestCfgRelation::Unknown,
    }
}

fn parse_cfg_attr_predicate(list: &syn::MetaList) -> TestCfgRelation {
    match parse_meta_arguments(list) {
        Ok(arguments) if arguments.len() >= 2 => cfg_relation(&arguments[0]),
        _ => TestCfgRelation::Unknown,
    }
}

fn parse_meta_arguments(list: &syn::MetaList) -> syn::Result<Punctuated<Meta, syn::Token![,]>> {
    Punctuated::<Meta, syn::Token![,]>::parse_terminated.parse2(list.tokens.clone())
}

fn cfg_relation(meta: &Meta) -> TestCfgRelation {
    match meta {
        Meta::Path(path) if path.is_ident("test") => TestCfgRelation::Positive,
        Meta::Path(_) | Meta::NameValue(_) => TestCfgRelation::Unrelated,
        Meta::List(list) if list.path.is_ident("not") => match parse_cfg_predicate(list) {
            TestCfgRelation::Positive => TestCfgRelation::Negative,
            TestCfgRelation::Negative => TestCfgRelation::Positive,
            TestCfgRelation::Unrelated => TestCfgRelation::Unrelated,
            TestCfgRelation::Unknown => TestCfgRelation::Unknown,
        },
        Meta::List(list) if list.path.is_ident("any") => combine_any(parse_meta_arguments(list)),
        Meta::List(list) if list.path.is_ident("all") => combine_all(parse_meta_arguments(list)),
        Meta::List(_) => TestCfgRelation::Unknown,
    }
}

fn combine_any(arguments: syn::Result<Punctuated<Meta, syn::Token![,]>>) -> TestCfgRelation {
    let Ok(arguments) = arguments else {
        return TestCfgRelation::Unknown;
    };
    let relations = arguments.iter().map(cfg_relation).collect::<Vec<_>>();
    if relations.contains(&TestCfgRelation::Positive) {
        TestCfgRelation::Positive
    } else if relations.contains(&TestCfgRelation::Unknown) {
        TestCfgRelation::Unknown
    } else if !relations.is_empty()
        && relations
            .iter()
            .all(|relation| *relation == TestCfgRelation::Negative)
    {
        TestCfgRelation::Negative
    } else {
        TestCfgRelation::Unrelated
    }
}

fn combine_all(arguments: syn::Result<Punctuated<Meta, syn::Token![,]>>) -> TestCfgRelation {
    let Ok(arguments) = arguments else {
        return TestCfgRelation::Unknown;
    };
    let relations = arguments.iter().map(cfg_relation).collect::<Vec<_>>();
    if relations.contains(&TestCfgRelation::Negative) {
        TestCfgRelation::Negative
    } else if relations.contains(&TestCfgRelation::Positive) {
        TestCfgRelation::Positive
    } else if relations.contains(&TestCfgRelation::Unknown) {
        TestCfgRelation::Unknown
    } else {
        TestCfgRelation::Unrelated
    }
}

#[test]
fn detector_catches_whitespace_ufcs_alias_macro_and_native_store_access() {
    let source = r#"
        #[test]
        fn bypass_attempts() {
            use keyring::Entry as Credential;
            let entry = Credential::new("service", "account").unwrap();
            let _ = entry . set_password ("secret");
            let _ = keyring::Entry::get_password(&entry);
            invoke!(entry.delete_credential());
            let store = wokcore_storage::NativeSecretStore::new();
            let _ = store.get(&secret_ref);
        }
    "#;

    let violations = detect_forbidden_native_access(source, true).unwrap();

    for expected in [
        "Entry",
        "set_password",
        "get_password",
        "delete_credential",
        "NativeSecretStore",
    ] {
        assert!(
            violations.iter().any(|violation| violation == expected),
            "detector missed {expected}: {violations:?}"
        );
    }
}

#[test]
fn detector_ignores_fixture_strings_containing_forbidden_spellings() {
    let source = r#"
        #[test]
        fn detector_fixtures() {
            let fixtures = [
                "keyring::Entry",
                "NativeSecretStore",
                "set_password",
                "get_password",
                "delete_credential",
            ];
            assert_eq!(fixtures.len(), 5);
        }
    "#;

    assert!(
        detect_forbidden_native_access(source, true)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scanner_covers_integration_cfg_test_tests_suffix_and_production_files() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("tests/nested")).unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("tests/nested/added.rs"),
        "#[test] fn added() { keyring::Entry::new(\"s\", \"a\"); }",
    )
    .unwrap();
    fs::write(
        directory.path().join("src/inline.rs"),
        r#"
            fn production_is_scanned() { keyring::Entry::new("s", "a"); }
            #[cfg(test)]
            mod tests {
                #[test]
                fn unit() { native.delete_credential(); }
            }
            struct Fixture;
            impl Fixture {
                #[cfg(test)]
                fn cfg_method() { native.set_password("secret"); }
            }
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("src/separate_tests.rs"),
        "fn separate() { wokcore_storage::NativeSecretStore::new(); }",
    )
    .unwrap();

    let violations = scan_crate_test_sources(directory.path()).unwrap();

    for expected_file in ["added.rs", "inline.rs", "separate_tests.rs"] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(expected_file)),
            "scanner missed {expected_file}: {violations:?}"
        );
    }
    assert_eq!(violations.len(), 5);
}

#[test]
fn crate_tests_never_access_real_native_credentials() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let violations = scan_crate_test_sources(manifest_dir).unwrap();

    assert!(
        violations.is_empty(),
        "test code accesses native credentials:\n{}",
        violations.join("\n")
    );
}

#[test]
fn scanner_follows_external_test_modules_recursive_helpers_and_path_modules() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::create_dir_all(directory.path().join("tests")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"
            #[cfg(test)]
            mod external;
            #[cfg(test)]
            #[path = "../shared/helper.rs"]
            mod custom;
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("src/external.rs"),
        r#"
            fn unmarked_helper() { credential.set_password("secret"); }
            #[path = "../shared/nested.rs"]
            mod nested;
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/nested.rs"),
        r#"fn recursive_helper() { keyring::Entry::new("service", "account"); }"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/helper.rs"),
        r#"fn path_helper() { native.delete_credential(); }"#,
    )
    .unwrap();

    let violations = scan_crate_test_sources(directory.path()).unwrap();

    for expected_file in ["external.rs", "nested.rs", "helper.rs"] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(expected_file)),
            "module graph missed {expected_file}: {violations:?}"
        );
    }
}

#[test]
fn detector_covers_cfg_test_locals_statements_expressions_and_cfg_attr() {
    let source = r#"
        fn production_container() {
            #[cfg(test)]
            let credential = keyring::Entry::new("service", "account");
            #[cfg(test)]
            {
                credential.set_password("secret");
            }
            #[cfg_attr(test, allow(unused_variables))]
            let store = wokcore_storage::NativeSecretStore::new();
        }
    "#;

    let violations = detect_forbidden_native_access(source, false).unwrap();

    for expected in ["Entry", "set_password", "NativeSecretStore"] {
        assert!(
            violations.iter().any(|violation| violation == expected),
            "cfg detector missed {expected}: {violations:?}"
        );
    }
}

#[test]
fn cfg_not_test_is_not_classified_as_test_code() {
    let source = r#"
        #[cfg(not(test))]
        fn production_only() {
            keyring::Entry::new("service", "account");
        }
    "#;

    assert!(
        detect_forbidden_native_access(source, false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cfg_any_all_and_unknown_predicates_fail_closed_for_test_reachable_code() {
    let source = r#"
        #[cfg(any(test, windows))]
        fn any_test_path() { keyring::Entry::new("service", "account"); }

        #[cfg(all(unix, test))]
        fn all_test_path() { credential.set_password("secret"); }

        #[cfg(custom_predicate(test))]
        fn unknown_test_predicate() {
            wokcore_storage::NativeSecretStore::new();
        }
    "#;

    let violations = detect_forbidden_native_access(source, false).unwrap();

    for expected in ["Entry", "set_password", "NativeSecretStore"] {
        assert!(
            violations.iter().any(|violation| violation == expected),
            "cfg predicate handling missed {expected}: {violations:?}"
        );
    }
}

#[test]
fn scanner_fails_closed_when_a_test_reachable_module_cannot_be_resolved() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        "#[cfg(not(test))] mod production_only;",
    )
    .unwrap();
    fs::write(
        directory.path().join("src/production_only.rs"),
        "fn harmless_production_module() {}",
    )
    .unwrap();

    assert!(
        scan_crate_test_sources(directory.path())
            .unwrap()
            .is_empty()
    );

    fs::write(
        directory.path().join("src/lib.rs"),
        "#[cfg(any(test, windows))] mod missing;",
    )
    .unwrap();
    let error = scan_crate_test_sources(directory.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("could not be resolved"));
}

#[test]
fn source_policy_allows_only_exact_native_shapes_and_reexports() {
    let exact_reexport = syn::parse_file("pub use secrets::NativeSecretStore;").unwrap();
    assert!(detect_forbidden_production_access(Path::new("lib.rs"), &exact_reexport).is_empty());

    for mutated_reexport in [
        "pub(crate) use secrets::NativeSecretStore;",
        "pub use secrets::NativeSecretStore as NativeSecretStore;",
        "pub use other::NativeSecretStore;",
    ] {
        let syntax = syn::parse_file(mutated_reexport).unwrap();
        assert!(
            !detect_forbidden_production_access(Path::new("lib.rs"), &syntax).is_empty(),
            "source policy allowed mutated reexport: {mutated_reexport}"
        );
    }

    let exact_boundary = syn::parse_file(
        r#"
            use keyring::{Entry, Error as KeyringError};
            pub struct NativeSecretStore;
            impl NativeSecretStore {
                pub const fn new() -> Self { Self }
            }
            impl SecretStore for NativeSecretStore {
                fn get() {
                    let entry = Entry::new("service", "account").unwrap();
                    entry.get_password();
                }
            }
        "#,
    )
    .unwrap();
    assert!(
        detect_forbidden_production_access(Path::new("secrets/native.rs"), &exact_boundary)
            .is_empty()
    );

    let mutated_boundary = syn::parse_file(
        r#"
            use keyring::{Entry, Error as KeyringError};
            pub struct NativeSecretStore { private: () }
            impl NativeSecretStore {
                pub const fn new() -> Self { Self { private: () } }
            }
            impl SecretStore for NativeSecretStore {}
        "#,
    )
    .unwrap();
    assert!(
        !detect_forbidden_production_access(Path::new("secrets/native.rs"), &mutated_boundary)
            .is_empty()
    );
}

#[test]
fn source_policy_rejects_production_wrappers_and_hidden_native_factories() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src/secrets")).unwrap();
    fs::create_dir_all(directory.path().join("tests")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        "pub use secrets::NativeSecretStore;",
    )
    .unwrap();
    fs::write(
        directory.path().join("src/secrets/mod.rs"),
        "pub use native::NativeSecretStore;",
    )
    .unwrap();
    fs::write(
        directory.path().join("src/secrets/native.rs"),
        r#"
            use keyring::{Entry, Error as KeyringError};
            pub struct NativeSecretStore;
            impl NativeSecretStore {
                pub const fn new() -> Self { Self }
            }
            impl SecretStore for NativeSecretStore {
                fn allowed_boundary() {
                    let entry = Entry::new("service", "account").unwrap();
                    entry.get_password();
                }
            }
            fn hidden_factory() -> NativeSecretStore {
                NativeSecretStore::new()
            }
            macro_rules! hidden_native_wrapper {
                () => { keyring::Entry::new("service", "account") };
            }
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("src/wrapper.rs"),
        "fn production_wrapper() { credential.delete_credential(); }",
    )
    .unwrap();

    let violations = scan_crate_test_sources(directory.path()).unwrap();

    for expected_file in ["native.rs", "wrapper.rs"] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(expected_file)),
            "source policy missed {expected_file}: {violations:?}"
        );
    }
    assert!(
        !violations
            .iter()
            .any(|violation| violation.contains("lib.rs"))
    );
    assert!(
        !violations
            .iter()
            .any(|violation| violation.contains("secrets\\mod.rs"))
    );
}

#[test]
fn scanner_follows_production_path_modules_outside_the_src_tree() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"#[path = "../shared/wrapper.rs"] mod wrapper;"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/wrapper.rs"),
        r#"
            fn direct_native_access() {
                keyring::Entry::new("service", "account");
            }
            #[path = "nested.rs"]
            mod nested;
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/nested.rs"),
        r#"
            macro_rules! hidden_native_wrapper {
                () => { credential.delete_credential() };
            }
        "#,
    )
    .unwrap();

    let violations = scan_crate_test_sources(directory.path()).unwrap();

    for (expected_file, expected) in [("wrapper.rs", "Entry"), ("nested.rs", "delete_credential")] {
        assert!(
            violations.iter().any(|violation| {
                violation.contains(expected_file) && violation.ends_with(expected)
            }),
            "production module graph missed {expected_file}: {expected}: {violations:?}"
        );
    }
}

#[test]
fn scanner_uses_cfg_attr_test_path_and_rejects_ambiguous_path_shapes() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"#[cfg_attr(test, path = "../shared/test_wrapper.rs")] mod wrapper;"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("src/wrapper.rs"),
        "fn harmless_default_module() {}",
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/test_wrapper.rs"),
        r#"fn test_only_wrapper() { credential.set_password("secret"); }"#,
    )
    .unwrap();

    let violations = scan_crate_test_sources(directory.path()).unwrap();

    assert!(
        violations.iter().any(|violation| {
            violation.contains("test_wrapper.rs") && violation.ends_with("set_password")
        }),
        "cfg_attr(test, path) resolved the wrong module: {violations:?}"
    );

    fs::write(
        directory.path().join("src/lib.rs"),
        r#"
            #[path = "wrapper.rs"]
            #[cfg_attr(test, path = "../shared/test_wrapper.rs")]
            mod wrapper;
        "#,
    )
    .unwrap();
    let ambiguous = scan_crate_test_sources(directory.path()).unwrap_err();
    assert_eq!(ambiguous.kind(), io::ErrorKind::InvalidData);
    assert!(ambiguous.to_string().contains("ambiguous"));

    fs::write(
        directory.path().join("src/lib.rs"),
        r#"
            #[cfg_attr(test, cfg_attr(test, path = "../shared/test_wrapper.rs"))]
            mod wrapper;
        "#,
    )
    .unwrap();
    let unsupported = scan_crate_test_sources(directory.path()).unwrap_err();
    assert_eq!(unsupported.kind(), io::ErrorKind::InvalidData);
    assert!(unsupported.to_string().contains("unsupported cfg_attr"));
}

#[test]
fn scanner_follows_literal_include_files_recursively() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"
            #[cfg(test)]
            mod tests {
                include!(r"../shared/helper.inc");
            }
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/helper.inc"),
        r#"
            fn included_helper() {
                wokcore_storage::NativeSecretStore::new();
            }
            include!("nested.rs");
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/nested.rs"),
        r#"fn recursively_included_helper() { keyring::Entry::new("service", "account"); }"#,
    )
    .unwrap();

    let violations = scan_crate_test_sources(directory.path()).unwrap();

    for (expected_file, expected) in [("helper.inc", "NativeSecretStore"), ("nested.rs", "Entry")] {
        assert!(
            violations.iter().any(|violation| {
                violation.contains(expected_file) && violation.ends_with(expected)
            }),
            "include graph missed {expected_file}: {expected}: {violations:?}"
        );
    }
}

#[test]
fn scanner_follows_raw_identifier_direct_include_macros() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"r#include!("../shared/helper.inc");"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/helper.inc"),
        r#"fn included_helper() { keyring::Entry::new("service", "account"); }"#,
    )
    .unwrap();

    let violations = scan_crate_test_sources(directory.path()).unwrap();

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("helper.inc") && violation.ends_with("Entry")),
        "raw direct include graph was missed: {violations:?}"
    );
}

#[test]
fn scanner_rejects_nonliteral_missing_and_ambiguous_includes() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();

    for (source, expected) in [
        (
            r#"const HELPER: &str = "helper.inc"; include!(HELPER);"#,
            "string literal",
        ),
        (r#"include!("missing.inc");"#, "failed to resolve"),
        (
            r#"include!("first.inc", "second.inc");"#,
            "single string literal",
        ),
        (
            r#"include!(concat!("../shared/", "helper.inc"));"#,
            "string literal",
        ),
        (r#"include!(env!("OUT_DIR"));"#, "string literal"),
    ] {
        fs::write(directory.path().join("src/lib.rs"), source).unwrap();
        let error = scan_crate_test_sources(directory.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains(expected),
            "unexpected include error for {source}: {error}"
        );
    }
}

#[test]
fn scanner_rejects_qualified_include_macros() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"std::include!("../shared/helper.inc");"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/helper.inc"),
        "fn included_helper() {}",
    )
    .unwrap();

    let error = scan_crate_test_sources(directory.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("qualified include"));
}

#[test]
fn scanner_rejects_raw_identifier_qualified_include_macros() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"std::r#include!("../shared/helper.inc");"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/helper.inc"),
        "fn included_helper() {}",
    )
    .unwrap();

    let error = scan_crate_test_sources(directory.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("qualified include"));
}

#[test]
fn scanner_rejects_imported_and_aliased_include_macros() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("shared/helper.inc"),
        "fn included_helper() {}",
    )
    .unwrap();

    for source in [
        r#"use std::include; include!("../shared/helper.inc");"#,
        r#"use std::include as load; load!("../shared/helper.inc");"#,
    ] {
        fs::write(directory.path().join("src/lib.rs"), source).unwrap();
        let error = scan_crate_test_sources(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("include import"),
            "unexpected include import result for {source}: {error}"
        );
    }
}

#[test]
fn scanner_rejects_raw_identifier_imported_and_aliased_include_macros() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::create_dir_all(directory.path().join("shared")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"
            use std::r#include as load;
            load!("../shared/helper.inc");
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("shared/helper.inc"),
        "fn included_helper() {}",
    )
    .unwrap();

    let error = scan_crate_test_sources(directory.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("include import"));
}

#[test]
fn scanner_rejects_macro_rules_that_can_generate_include_invocations() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();

    for source in [
        r#"macro_rules! load { () => { include!("helper.inc"); } }"#,
        r#"macro_rules! load { () => { std::include!("helper.inc"); } }"#,
    ] {
        fs::write(directory.path().join("src/lib.rs"), source).unwrap();
        let error = scan_crate_test_sources(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("macro_rules") && error.to_string().contains("include"),
            "unexpected macro_rules result for {source}: {error}"
        );
    }
}

#[test]
fn scanner_rejects_macro_rules_with_raw_identifier_include_invocations() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"macro_rules! load { () => { r#include!("helper.inc"); } }"#,
    )
    .unwrap();

    let error = scan_crate_test_sources(directory.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("macro_rules") && error.to_string().contains("include"));
}

#[test]
fn scanner_ignores_include_spellings_inside_literals() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"
            /// Documentation mentioning `std::include!("helper.inc")`.
            fn harmless() {
                let message = "macro_rules! { include!(\"helper.inc\") }";
                assert!(message.contains("include"));
            }
        "#,
    )
    .unwrap();

    assert!(
        scan_crate_test_sources(directory.path())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scanner_rejects_includes_outside_the_crate_and_include_cycles() {
    let directory = tempfile::tempdir().unwrap();
    let crate_dir = directory.path().join("crate");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(
        directory.path().join("outside.inc"),
        "fn outside_crate() {}",
    )
    .unwrap();
    fs::write(
        crate_dir.join("src/lib.rs"),
        r#"include!("../../outside.inc");"#,
    )
    .unwrap();

    let outside = scan_crate_test_sources(&crate_dir).unwrap_err();
    assert_eq!(outside.kind(), io::ErrorKind::InvalidData);
    assert!(outside.to_string().contains("outside crate root"));

    fs::write(crate_dir.join("src/lib.rs"), r#"include!("first.inc");"#).unwrap();
    fs::write(
        crate_dir.join("src/first.inc"),
        r#"include!("second.inc");"#,
    )
    .unwrap();
    fs::write(
        crate_dir.join("src/second.inc"),
        r#"include!("first.inc");"#,
    )
    .unwrap();

    let cycle = scan_crate_test_sources(&crate_dir).unwrap_err();
    assert_eq!(cycle.kind(), io::ErrorKind::InvalidData);
    assert!(cycle.to_string().contains("include cycle"));

    fs::write(crate_dir.join("shared.inc"), "fn shared() {}").unwrap();
    fs::write(
        crate_dir.join("src/lib.rs"),
        r#"
            include!("../shared.inc");
            #[cfg(test)]
            mod tests {
                include!("../shared.inc");
            }
        "#,
    )
    .unwrap();

    let ambiguous = scan_crate_test_sources(&crate_dir).unwrap_err();
    assert_eq!(ambiguous.kind(), io::ErrorKind::InvalidData);
    assert!(ambiguous.to_string().contains("both production and test"));
}

#[test]
fn storage_library_disables_doctests_outside_the_scanned_source_graph() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(manifest_path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();

    assert_eq!(manifest["lib"]["doctest"].as_bool(), Some(false));
}
