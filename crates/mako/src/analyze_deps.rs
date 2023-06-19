use swc_css_ast::{ImportHref, Url, UrlValue};
use swc_css_visit::VisitWith as CSSVisitWith;
use swc_ecma_ast::{CallExpr, Callee, Expr, Import, Lit, ModuleDecl};
use swc_ecma_visit::{Visit, VisitWith};

use crate::module::{Dependency, ModuleAst, ResolveType};

pub fn analyze_deps(ast: &ModuleAst) -> Vec<Dependency> {
    match ast {
        ModuleAst::Script(ast) => analyze_deps_js(ast),
        ModuleAst::Css(ast) => analyze_deps_css(ast),
        _ => {
            vec![]
        }
    }
}

pub fn analyze_deps_js(ast: &swc_ecma_ast::Module) -> Vec<Dependency> {
    let mut visitor = DepCollectVisitor::new();
    ast.visit_with(&mut visitor);
    visitor.dependencies
}

fn analyze_deps_css(ast: &swc_css_ast::Stylesheet) -> Vec<Dependency> {
    let mut visitor = DepCollectVisitor::new();
    ast.visit_with(&mut visitor);
    visitor.dependencies
}

struct DepCollectVisitor {
    dependencies: Vec<Dependency>,
    dep_strs: Vec<String>,
    order: usize,
}

impl DepCollectVisitor {
    fn new() -> Self {
        Self {
            dependencies: vec![],
            dep_strs: vec![],
            // start with 1
            // 0 for swc helpers
            order: 1,
        }
    }
    fn bind_dependency(&mut self, source: String, resolve_type: ResolveType) {
        if !self.dep_strs.contains(&source) {
            self.dep_strs.push(source.clone());
            self.dependencies.push(Dependency {
                source,
                order: self.order,
                resolve_type,
            });
            self.order += 1;
        }
    }
}

impl Visit for DepCollectVisitor {
    fn visit_module_decl(&mut self, n: &ModuleDecl) {
        match n {
            ModuleDecl::Import(import) => {
                if import.type_only {
                    return;
                }
                let src = import.src.value.to_string();
                self.bind_dependency(src, ResolveType::Import);
            }
            ModuleDecl::ExportNamed(export) => {
                if let Some(src) = &export.src {
                    let src = src.value.to_string();
                    self.bind_dependency(src, ResolveType::ExportNamed);
                }
            }
            ModuleDecl::ExportAll(export) => {
                let src = export.src.value.to_string();
                self.bind_dependency(src, ResolveType::ExportAll);
            }
            _ => {}
        }
        // export function xxx() {} 里可能包含 require 或 import()
        n.visit_children_with(self);
    }
    fn visit_call_expr(&mut self, expr: &CallExpr) {
        if is_commonjs_require(expr) {
            if let Some(src) = get_first_arg_str(expr) {
                self.bind_dependency(src, ResolveType::Require);
                return;
            }
        } else if is_dynamic_import(expr) {
            if let Some(src) = get_first_arg_str(expr) {
                self.bind_dependency(src, ResolveType::DynamicImport);
                return;
            }
        }
        expr.visit_children_with(self);
    }
}

impl swc_css_visit::Visit for DepCollectVisitor {
    fn visit_import_href(&mut self, n: &ImportHref) {
        match n {
            // e.g.
            // @import url(a.css)
            // @import url("a.css")
            ImportHref::Url(url) => {
                let src: Option<String> = url.value.as_ref().map(|box value| match value {
                    UrlValue::Str(str) => str.value.to_string(),
                    UrlValue::Raw(raw) => raw.value.to_string(),
                });
                if let Some(src) = src {
                    self.bind_dependency(src, ResolveType::Css);
                }
            }
            // e.g.
            // @import "a.css"
            ImportHref::Str(src) => {
                let src = src.value.to_string();
                self.bind_dependency(src, ResolveType::Css);
            }
        }
        // remove visit children since it is not used currently
        // n.visit_children_with(self);
    }

    fn visit_url(&mut self, n: &Url) {
        // 检查 url()
        let href_string = n
            .value
            .as_ref()
            .map(|box value| match value {
                UrlValue::Str(str) => str.value.to_string(),
                UrlValue::Raw(raw) => raw.value.to_string(),
            })
            .unwrap();
        self.bind_dependency(href_string, ResolveType::Css);
        // n.visit_children_with(self);
    }
}

pub fn is_dynamic_import(call_expr: &CallExpr) -> bool {
    matches!(&call_expr.callee, Callee::Import(Import { .. }))
}

pub fn is_commonjs_require(call_expr: &CallExpr) -> bool {
    if let Callee::Expr(box Expr::Ident(swc_ecma_ast::Ident { sym, .. })) = &call_expr.callee {
        sym == "require"
    } else {
        false
    }
}

fn get_first_arg_str(call_expr: &CallExpr) -> Option<String> {
    if let Some(arg) = call_expr.args.first() {
        if let box Expr::Lit(Lit::Str(str_)) = &arg.expr {
            return Some(str_.value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, RwLock};

    use super::analyze_deps_js;
    use crate::ast::build_js_ast;
    use crate::chunk_graph::ChunkGraph;
    use crate::compiler::{Context, Meta};
    use crate::module_graph::ModuleGraph;

    #[test]
    fn test_analyze_deps() {
        let deps = resolve(
            r#"
import 'foo';
            "#
            .trim(),
        );
        assert_eq!(deps, "foo");
    }

    #[test]
    fn test_analyze_deps_inside_exports() {
        let deps = resolve(
            r#"
export function foo() {
    require('foo');
}
            "#
            .trim(),
        );
        assert_eq!(deps, "foo");
    }

    #[test]
    fn test_analyze_deps_ignore_type_only() {
        let deps = resolve(
            r#"
import type { x } from 'foo';
import 'bar';
            "#
            .trim(),
        );
        assert_eq!(deps, "bar");
    }

    fn resolve(code: &str) -> String {
        let root = PathBuf::from("/path/to/root");
        let ast = build_js_ast(
            "test.ts",
            code,
            &Arc::new(Context {
                config: Default::default(),
                root,
                module_graph: RwLock::new(ModuleGraph::new()),
                chunk_graph: RwLock::new(ChunkGraph::new()),
                assets_info: Mutex::new(HashMap::new()),
                meta: Meta::new(),
            }),
        )
        .unwrap();
        let mut deps = vec![];
        deps.extend(analyze_deps_js(&ast));
        let deps = deps
            .iter()
            .map(|dep| dep.source.as_str())
            .collect::<Vec<_>>()
            .join(",");
        deps
    }
}
