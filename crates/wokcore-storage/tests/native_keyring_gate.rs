use std::{
    fs, io,
    path::{Path, PathBuf},
};

use proc_macro2::{TokenStream, TokenTree};
use syn::{
    Attribute, ForeignItem, ImplItem, Item, Macro, Meta, Path as SyntaxPath, TraitItem, UseTree,
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
    let mut sources = Vec::new();
    collect_rust_files(&manifest_dir.join("tests"), &mut sources)?;
    collect_rust_files(&manifest_dir.join("src"), &mut sources)?;

    let tests_dir = manifest_dir.join("tests");
    let mut violations = Vec::new();
    for path in sources {
        let source = fs::read_to_string(&path)?;
        let scan_entire_file = path.starts_with(&tests_dir)
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_tests.rs"));
        let detected =
            detect_forbidden_native_access(&source, scan_entire_file).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to parse {}: {error}", path.display()),
                )
            })?;
        violations.extend(
            detected
                .into_iter()
                .map(|violation| format!("{}: {violation}", path.display())),
        );
    }
    Ok(violations)
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

#[derive(Default)]
struct ForbiddenNativeAccess {
    violations: Vec<String>,
}

impl ForbiddenNativeAccess {
    fn check_identifier(&mut self, identifier: &syn::Ident) {
        let identifier = identifier.to_string();
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
    fn visit_expr_method_call(&mut self, expression: &'syntax syn::ExprMethodCall) {
        self.check_identifier(&expression.method);
        visit::visit_expr_method_call(self, expression);
    }

    fn visit_path(&mut self, path: &'syntax SyntaxPath) {
        for segment in &path.segments {
            self.check_identifier(&segment.ident);
        }
        visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'syntax UseTree) {
        match tree {
            UseTree::Path(path) => {
                self.check_identifier(&path.ident);
                self.visit_use_tree(&path.tree);
            }
            UseTree::Name(name) => self.check_identifier(&name.ident),
            UseTree::Rename(rename) => {
                self.check_identifier(&rename.ident);
                self.check_identifier(&rename.rename);
            }
            UseTree::Group(group) => {
                for item in &group.items {
                    self.visit_use_tree(item);
                }
            }
            UseTree::Glob(_) => {}
        }
    }

    fn visit_macro(&mut self, item: &'syntax Macro) {
        self.visit_path(&item.path);
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

fn attribute_marks_test_code(attribute: &Attribute) -> bool {
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
            tokens_contain_identifier(list.tokens.clone(), "test")
        }
        _ => false,
    }
}

fn tokens_contain_identifier(tokens: TokenStream, expected: &str) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(identifier) => identifier == expected,
        TokenTree::Group(group) => tokens_contain_identifier(group.stream(), expected),
        TokenTree::Punct(_) | TokenTree::Literal(_) => false,
    })
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
fn scanner_covers_integration_cfg_test_and_tests_suffix_files_only() {
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
            fn production_is_not_tested() { keyring::Entry::new("s", "a"); }
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
    assert_eq!(violations.len(), 4);
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
