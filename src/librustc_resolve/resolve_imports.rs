// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use self::Determinacy::*;
use self::ImportDirectiveSubclass::*;

use Module;
use Namespace::{self, TypeNS, ValueNS};
use {NameBinding, NameBindingKind, PrivacyError, ToNameBinding};
use ResolveResult;
use ResolveResult::*;
use Resolver;
use UseLexicalScopeFlag::DontUseLexicalScope;
use {names_to_string, module_to_string};
use {resolve_error, ResolutionError};

use rustc::ty;
use rustc::lint::builtin::PRIVATE_IN_PUBLIC;
use rustc::hir::def::*;

use syntax::ast::{NodeId, Name};
use syntax::util::lev_distance::find_best_match_for_name;
use syntax_pos::{Span, DUMMY_SP};

use std::cell::{Cell, RefCell};

impl<'a> Resolver<'a> {
    pub fn resolve_imports(&mut self) {
        ImportResolver { resolver: self }.resolve_imports();
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Determinacy {
    Determined,
    Undetermined,
}

/// Contains data for specific types of import directives.
#[derive(Clone, Debug)]
pub enum ImportDirectiveSubclass<'a> {
    SingleImport {
        target: Name,
        source: Name,
        value_result: Cell<Result<&'a NameBinding<'a>, Determinacy>>,
        type_result: Cell<Result<&'a NameBinding<'a>, Determinacy>>,
    },
    GlobImport { is_prelude: bool },
}

impl<'a> ImportDirectiveSubclass<'a> {
    pub fn single(target: Name, source: Name) -> Self {
        SingleImport {
            target: target,
            source: source,
            type_result: Cell::new(Err(Undetermined)),
            value_result: Cell::new(Err(Undetermined)),
        }
    }
}

/// One import directive.
#[derive(Debug,Clone)]
pub struct ImportDirective<'a> {
    pub id: NodeId,
    parent: Module<'a>,
    module_path: Vec<Name>,
    imported_module: Cell<Option<Module<'a>>>, // the resolution of `module_path`
    subclass: ImportDirectiveSubclass<'a>,
    span: Span,
    vis: Cell<ty::Visibility>,
}

impl<'a> ImportDirective<'a> {
    pub fn is_glob(&self) -> bool {
        match self.subclass { ImportDirectiveSubclass::GlobImport { .. } => true, _ => false }
    }
}

#[derive(Clone, Default)]
/// Records information about the resolution of a name in a namespace of a module.
pub struct NameResolution<'a> {
    /// The single imports that define the name in the namespace.
    single_imports: SingleImports<'a>,
    /// The least shadowable known binding for this name, or None if there are no known bindings.
    pub binding: Option<&'a NameBinding<'a>>,
    duplicate_globs: Vec<&'a NameBinding<'a>>,
}

#[derive(Clone, Debug)]
enum SingleImports<'a> {
    /// No single imports can define the name in the namespace.
    None,
    /// Only the given single import can define the name in the namespace.
    MaybeOne(&'a ImportDirective<'a>),
    /// At least one single import will define the name in the namespace.
    AtLeastOne,
}

impl<'a> Default for SingleImports<'a> {
    fn default() -> Self {
        SingleImports::None
    }
}

impl<'a> SingleImports<'a> {
    fn add_directive(&mut self, directive: &'a ImportDirective<'a>) {
        match *self {
            SingleImports::None => *self = SingleImports::MaybeOne(directive),
            // If two single imports can define the name in the namespace, we can assume that at
            // least one of them will define it since otherwise both would have to define only one
            // namespace, leading to a duplicate error.
            SingleImports::MaybeOne(_) => *self = SingleImports::AtLeastOne,
            SingleImports::AtLeastOne => {}
        };
    }

    fn directive_failed(&mut self) {
        match *self {
            SingleImports::None => unreachable!(),
            SingleImports::MaybeOne(_) => *self = SingleImports::None,
            SingleImports::AtLeastOne => {}
        }
    }
}

impl<'a> NameResolution<'a> {
    // Returns the binding for the name if it is known or None if it not known.
    fn binding(&self) -> Option<&'a NameBinding<'a>> {
        self.binding.and_then(|binding| match self.single_imports {
            SingleImports::None => Some(binding),
            _ if !binding.is_glob_import() => Some(binding),
            _ => None, // The binding could be shadowed by a single import, so it is not known.
        })
    }
}

impl<'a> Resolver<'a> {
    fn resolution(&self, module: Module<'a>, name: Name, ns: Namespace)
                  -> &'a RefCell<NameResolution<'a>> {
        *module.resolutions.borrow_mut().entry((name, ns))
               .or_insert_with(|| self.arenas.alloc_name_resolution())
    }

    /// Attempts to resolve the supplied name in the given module for the given namespace.
    /// If successful, returns the binding corresponding to the name.
    /// Invariant: if `record_used` is `Some`, import resolution must be complete.
    pub fn resolve_name_in_module(&mut self,
                                  module: Module<'a>,
                                  name: Name,
                                  ns: Namespace,
                                  allow_private_imports: bool,
                                  record_used: Option<Span>)
                                  -> ResolveResult<&'a NameBinding<'a>> {
        self.populate_module_if_necessary(module);

        let resolution = self.resolution(module, name, ns);
        let resolution = match resolution.borrow_state() {
            ::std::cell::BorrowState::Unused => resolution.borrow_mut(),
            _ => return Failed(None), // This happens when there is a cycle of imports
        };

        let is_disallowed_private_import = |binding: &NameBinding| {
            !allow_private_imports && !binding.is_pseudo_public() && binding.is_import()
        };

        if let Some(span) = record_used {
            if let Some(binding) = resolution.binding {
                if is_disallowed_private_import(binding) {
                    return Failed(None);
                }
                self.record_use(name, ns, binding);
                if !self.is_accessible(binding.vis) {
                    self.privacy_errors.push(PrivacyError(span, name, binding));
                }
            }

            return resolution.binding.map(Success).unwrap_or(Failed(None));
        }

        // If the resolution doesn't depend on glob definability, check privacy and return.
        if let Some(result) = self.try_result(&resolution, ns) {
            return result.and_then(|binding| {
                if self.is_accessible(binding.vis) && !is_disallowed_private_import(binding) {
                    Success(binding)
                } else {
                    Failed(None)
                }
            });
        }

        // Check if the globs are determined
        for directive in module.globs.borrow().iter() {
            if self.is_accessible(directive.vis.get()) {
                if let Some(module) = directive.imported_module.get() {
                    let result = self.resolve_name_in_module(module, name, ns, true, None);
                    if let Indeterminate = result {
                        return Indeterminate;
                    }
                } else {
                    return Indeterminate;
                }
            }
        }

        Failed(None)
    }

    // Returns Some(the resolution of the name), or None if the resolution depends
    // on whether more globs can define the name.
    fn try_result(&mut self, resolution: &NameResolution<'a>, ns: Namespace)
                  -> Option<ResolveResult<&'a NameBinding<'a>>> {
        match resolution.binding {
            Some(binding) if !binding.is_glob_import() =>
                return Some(Success(binding)), // Items and single imports are not shadowable.
            _ => {}
        };

        // Check if a single import can still define the name.
        match resolution.single_imports {
            SingleImports::AtLeastOne => return Some(Indeterminate),
            SingleImports::MaybeOne(directive) if self.is_accessible(directive.vis.get()) => {
                let module = match directive.imported_module.get() {
                    Some(module) => module,
                    None => return Some(Indeterminate),
                };
                let name = match directive.subclass {
                    SingleImport { source, .. } => source,
                    GlobImport { .. } => unreachable!(),
                };
                match self.resolve_name_in_module(module, name, ns, true, None) {
                    Failed(_) => {}
                    _ => return Some(Indeterminate),
                }
            }
            SingleImports::MaybeOne(_) | SingleImports::None => {},
        }

        resolution.binding.map(Success)
    }

    // Add an import directive to the current module.
    pub fn add_import_directive(&mut self,
                                module_path: Vec<Name>,
                                subclass: ImportDirectiveSubclass<'a>,
                                span: Span,
                                id: NodeId,
                                vis: ty::Visibility) {
        let current_module = self.current_module;
        let directive = self.arenas.alloc_import_directive(ImportDirective {
            parent: current_module,
            module_path: module_path,
            imported_module: Cell::new(None),
            subclass: subclass,
            span: span,
            id: id,
            vis: Cell::new(vis),
        });

        self.indeterminate_imports.push(directive);
        match directive.subclass {
            SingleImport { target, .. } => {
                for &ns in &[ValueNS, TypeNS] {
                    let mut resolution = self.resolution(current_module, target, ns).borrow_mut();
                    resolution.single_imports.add_directive(directive);
                }
            }
            // We don't add prelude imports to the globs since they only affect lexical scopes,
            // which are not relevant to import resolution.
            GlobImport { is_prelude: true } => {}
            GlobImport { .. } => self.current_module.globs.borrow_mut().push(directive),
        }
    }

    // Given a binding and an import directive that resolves to it,
    // return the corresponding binding defined by the import directive.
    fn import(&mut self, binding: &'a NameBinding<'a>, directive: &'a ImportDirective<'a>)
              -> NameBinding<'a> {
        NameBinding {
            kind: NameBindingKind::Import {
                binding: binding,
                directive: directive,
            },
            span: directive.span,
            vis: directive.vis.get(),
        }
    }

    // Define the name or return the existing binding if there is a collision.
    pub fn try_define<T>(&mut self, module: Module<'a>, name: Name, ns: Namespace, binding: T)
                         -> Result<(), &'a NameBinding<'a>>
        where T: ToNameBinding<'a>
    {
        let binding = self.arenas.alloc_name_binding(binding.to_name_binding());
        self.update_resolution(module, name, ns, |_, resolution| {
            if let Some(old_binding) = resolution.binding {
                if binding.is_glob_import() {
                    resolution.duplicate_globs.push(binding);
                } else if old_binding.is_glob_import() {
                    resolution.duplicate_globs.push(old_binding);
                    resolution.binding = Some(binding);
                } else {
                    return Err(old_binding);
                }
            } else {
                resolution.binding = Some(binding);
            }

            Ok(())
        })
    }

    // Use `f` to mutate the resolution of the name in the module.
    // If the resolution becomes a success, define it in the module's glob importers.
    fn update_resolution<T, F>(&mut self, module: Module<'a>, name: Name, ns: Namespace, f: F) -> T
        where F: FnOnce(&mut Resolver<'a>, &mut NameResolution<'a>) -> T
    {
        // Ensure that `resolution` isn't borrowed when defining in the module's glob importers,
        // during which the resolution might end up getting re-defined via a glob cycle.
        let (new_binding, t) = {
            let mut resolution = &mut *self.resolution(module, name, ns).borrow_mut();
            let was_known = resolution.binding().is_some();

            let t = f(self, resolution);

            if was_known { return t; }
            match resolution.binding() {
                Some(binding) => (binding, t),
                None => return t,
            }
        };

        // Define `new_binding` in `module`s glob importers.
        if new_binding.is_importable() && new_binding.is_pseudo_public() {
            for directive in module.glob_importers.borrow_mut().iter() {
                let imported_binding = self.import(new_binding, directive);
                let _ = self.try_define(directive.parent, name, ns, imported_binding);
            }
        }

        t
    }
}

struct ImportResolver<'a, 'b: 'a> {
    resolver: &'a mut Resolver<'b>,
}

impl<'a, 'b: 'a> ::std::ops::Deref for ImportResolver<'a, 'b> {
    type Target = Resolver<'b>;
    fn deref(&self) -> &Resolver<'b> {
        self.resolver
    }
}

impl<'a, 'b: 'a> ::std::ops::DerefMut for ImportResolver<'a, 'b> {
    fn deref_mut(&mut self) -> &mut Resolver<'b> {
        self.resolver
    }
}

impl<'a, 'b: 'a> ty::NodeIdTree for ImportResolver<'a, 'b> {
    fn is_descendant_of(&self, node: NodeId, ancestor: NodeId) -> bool {
        self.resolver.is_descendant_of(node, ancestor)
    }
}

impl<'a, 'b:'a> ImportResolver<'a, 'b> {
    // Import resolution
    //
    // This is a fixed-point algorithm. We resolve imports until our efforts
    // are stymied by an unresolved import; then we bail out of the current
    // module and continue. We terminate successfully once no more imports
    // remain or unsuccessfully when no forward progress in resolving imports
    // is made.

    fn set_current_module(&mut self, module: Module<'b>) {
        self.current_module = module;
        self.current_vis = ty::Visibility::Restricted({
            let normal_module = self.get_nearest_normal_module_parent_or_self(module);
            self.definitions.as_local_node_id(normal_module.def_id().unwrap()).unwrap()
        });
    }

    /// Resolves all imports for the crate. This method performs the fixed-
    /// point iteration.
    fn resolve_imports(&mut self) {
        let mut i = 0;
        let mut prev_num_indeterminates = self.indeterminate_imports.len() + 1;

        while self.indeterminate_imports.len() < prev_num_indeterminates {
            prev_num_indeterminates = self.indeterminate_imports.len();
            debug!("(resolving imports) iteration {}, {} imports left", i, prev_num_indeterminates);

            let mut imports = Vec::new();
            ::std::mem::swap(&mut imports, &mut self.indeterminate_imports);

            for import in imports {
                match self.resolve_import(&import) {
                    Failed(_) => self.determined_imports.push(import),
                    Indeterminate => self.indeterminate_imports.push(import),
                    Success(()) => self.determined_imports.push(import),
                }
            }

            i += 1;
        }

        for module in self.arenas.local_modules().iter() {
            self.finalize_resolutions_in(module);
        }

        let mut errors = false;
        for i in 0 .. self.determined_imports.len() {
            let import = self.determined_imports[i];
            if let Failed(err) = self.finalize_import(import) {
                errors = true;
                let (span, help) = match err {
                    Some((span, msg)) => (span, msg),
                    None => (import.span, String::new()),
                };

                // If the error is a single failed import then create a "fake" import
                // resolution for it so that later resolve stages won't complain.
                self.import_dummy_binding(import);
                let path = import_path_to_string(&import.module_path, &import.subclass);
                let error = ResolutionError::UnresolvedImport(Some((&path, &help)));
                resolve_error(self.resolver, span, error);
            }
        }

        // Report unresolved imports only if no hard error was already reported
        // to avoid generating multiple errors on the same import.
        if !errors {
            if let Some(import) = self.indeterminate_imports.iter().next() {
                let error = ResolutionError::UnresolvedImport(None);
                resolve_error(self.resolver, import.span, error);
            }
        }
    }

    // Define a "dummy" resolution containing a Def::Err as a placeholder for a
    // failed resolution
    fn import_dummy_binding(&mut self, directive: &'b ImportDirective<'b>) {
        if let SingleImport { target, .. } = directive.subclass {
            let dummy_binding = self.arenas.alloc_name_binding(NameBinding {
                kind: NameBindingKind::Def(Def::Err),
                span: DUMMY_SP,
                vis: ty::Visibility::Public,
            });
            let dummy_binding = self.import(dummy_binding, directive);

            let _ = self.try_define(directive.parent, target, ValueNS, dummy_binding.clone());
            let _ = self.try_define(directive.parent, target, TypeNS, dummy_binding);
        }
    }

    /// Attempts to resolve the given import. The return value indicates
    /// failure if we're certain the name does not exist, indeterminate if we
    /// don't know whether the name exists at the moment due to other
    /// currently-unresolved imports, or success if we know the name exists.
    /// If successful, the resolved bindings are written into the module.
    fn resolve_import(&mut self, directive: &'b ImportDirective<'b>) -> ResolveResult<()> {
        debug!("(resolving import for module) resolving import `{}::...` in `{}`",
               names_to_string(&directive.module_path),
               module_to_string(self.current_module));

        self.set_current_module(directive.parent);

        let module = if let Some(module) = directive.imported_module.get() {
            module
        } else {
            let vis = directive.vis.get();
            // For better failure detection, pretend that the import will not define any names
            // while resolving its module path.
            directive.vis.set(ty::Visibility::PrivateExternal);
            let result =
                self.resolve_module_path(&directive.module_path, DontUseLexicalScope, None);
            directive.vis.set(vis);

            match result {
                Success(module) => module,
                Indeterminate => return Indeterminate,
                Failed(err) => return Failed(err),
            }
        };

        directive.imported_module.set(Some(module));
        let (source, target, value_result, type_result) = match directive.subclass {
            SingleImport { source, target, ref value_result, ref type_result } =>
                (source, target, value_result, type_result),
            GlobImport { .. } => {
                self.resolve_glob_import(directive);
                return Success(());
            }
        };

        let mut indeterminate = false;
        for &(ns, result) in &[(ValueNS, value_result), (TypeNS, type_result)] {
            if let Err(Undetermined) = result.get() {
                result.set({
                    match self.resolve_name_in_module(module, source, ns, false, None) {
                        Success(binding) => Ok(binding),
                        Indeterminate => Err(Undetermined),
                        Failed(_) => Err(Determined),
                    }
                });
            } else {
                continue
            };

            match result.get() {
                Err(Undetermined) => indeterminate = true,
                Err(Determined) => {
                    self.update_resolution(directive.parent, target, ns, |_, resolution| {
                        resolution.single_imports.directive_failed()
                    });
                }
                Ok(binding) if !binding.is_importable() => {
                    let msg = format!("`{}` is not directly importable", target);
                    struct_span_err!(self.session, directive.span, E0253, "{}", &msg)
                        .span_label(directive.span, &format!("cannot be imported directly"))
                        .emit();
                    // Do not import this illegal binding. Import a dummy binding and pretend
                    // everything is fine
                    self.import_dummy_binding(directive);
                    return Success(());
                }
                Ok(binding) => {
                    let imported_binding = self.import(binding, directive);
                    let conflict = self.try_define(directive.parent, target, ns, imported_binding);
                    if let Err(old_binding) = conflict {
                        let binding = &self.import(binding, directive);
                        self.report_conflict(directive.parent, target, ns, binding, old_binding);
                    }
                }
            }
        }

        if indeterminate { Indeterminate } else { Success(()) }
    }

    fn finalize_import(&mut self, directive: &'b ImportDirective<'b>) -> ResolveResult<()> {
        self.set_current_module(directive.parent);

        let ImportDirective { ref module_path, span, .. } = *directive;
        let module_result = self.resolve_module_path(&module_path, DontUseLexicalScope, Some(span));
        let module = match module_result {
            Success(module) => module,
            Indeterminate => return Indeterminate,
            Failed(err) => return Failed(err),
        };

        let (name, value_result, type_result) = match directive.subclass {
            SingleImport { source, ref value_result, ref type_result, .. } =>
                (source, value_result.get(), type_result.get()),
            GlobImport { .. } if module.def_id() == directive.parent.def_id() => {
                // Importing a module into itself is not allowed.
                let msg = "Cannot glob-import a module into itself.".into();
                return Failed(Some((directive.span, msg)));
            }
            GlobImport { .. } => return Success(()),
        };

        for &(ns, result) in &[(ValueNS, value_result), (TypeNS, type_result)] {
            if let Ok(binding) = result {
                self.record_use(name, ns, binding);
            }
        }

        if value_result.is_err() && type_result.is_err() {
            let (value_result, type_result);
            value_result = self.resolve_name_in_module(module, name, ValueNS, false, Some(span));
            type_result = self.resolve_name_in_module(module, name, TypeNS, false, Some(span));

            return if let (Failed(_), Failed(_)) = (value_result, type_result) {
                let resolutions = module.resolutions.borrow();
                let names = resolutions.iter().filter_map(|(&(ref n, _), resolution)| {
                    if *n == name { return None; } // Never suggest the same name
                    match *resolution.borrow() {
                        NameResolution { binding: Some(_), .. } => Some(n),
                        NameResolution { single_imports: SingleImports::None, .. } => None,
                        _ => Some(n),
                    }
                });
                let lev_suggestion = match find_best_match_for_name(names, &name.as_str(), None) {
                    Some(name) => format!(". Did you mean to use `{}`?", name),
                    None => "".to_owned(),
                };
                let module_str = module_to_string(module);
                let msg = if &module_str == "???" {
                    format!("no `{}` in the root{}", name, lev_suggestion)
                } else {
                    format!("no `{}` in `{}`{}", name, module_str, lev_suggestion)
                };
                Failed(Some((directive.span, msg)))
            } else {
                // `resolve_name_in_module` reported a privacy error.
                self.import_dummy_binding(directive);
                Success(())
            }
        }

        match (value_result, type_result) {
            (Ok(binding), _) if !binding.pseudo_vis().is_at_least(directive.vis.get(), self) => {
                let msg = format!("`{}` is private, and cannot be reexported", name);
                let note_msg =
                    format!("consider marking `{}` as `pub` in the imported module", name);
                struct_span_err!(self.session, directive.span, E0364, "{}", &msg)
                    .span_note(directive.span, &note_msg)
                    .emit();
            }

            (_, Ok(binding)) if !binding.pseudo_vis().is_at_least(directive.vis.get(), self) => {
                if binding.is_extern_crate() {
                    let msg = format!("extern crate `{}` is private, and cannot be reexported \
                                       (error E0364), consider declaring with `pub`",
                                       name);
                    self.session.add_lint(PRIVATE_IN_PUBLIC, directive.id, directive.span, msg);
                } else {
                    struct_span_err!(self.session, directive.span, E0365,
                                     "`{}` is private, and cannot be reexported", name)
                        .span_label(directive.span, &format!("reexport of private `{}`", name))
                        .note(&format!("consider declaring type or module `{}` with `pub`", name))
                        .emit();
                }
            }

            _ => {}
        }

        // Record what this import resolves to for later uses in documentation,
        // this may resolve to either a value or a type, but for documentation
        // purposes it's good enough to just favor one over the other.
        let def = match type_result.ok().and_then(NameBinding::def) {
            Some(def) => def,
            None => value_result.ok().and_then(NameBinding::def).unwrap(),
        };
        let path_resolution = PathResolution::new(def);
        self.def_map.insert(directive.id, path_resolution);

        debug!("(resolving single import) successfully resolved import");
        return Success(());
    }

    fn resolve_glob_import(&mut self, directive: &'b ImportDirective<'b>) {
        let module = directive.imported_module.get().unwrap();
        self.populate_module_if_necessary(module);

        if let Some(Def::Trait(_)) = module.def {
            self.session.span_err(directive.span, "items in traits are not importable.");
        }

        if module.def_id() == directive.parent.def_id()  {
            return;
        } else if let GlobImport { is_prelude: true } = directive.subclass {
            self.prelude = Some(module);
            return;
        }

        // Add to module's glob_importers
        module.glob_importers.borrow_mut().push(directive);

        // Ensure that `resolutions` isn't borrowed during `try_define`,
        // since it might get updated via a glob cycle.
        let bindings = module.resolutions.borrow().iter().filter_map(|(name, resolution)| {
            resolution.borrow().binding().map(|binding| (*name, binding))
        }).collect::<Vec<_>>();
        for ((name, ns), binding) in bindings {
            if binding.is_importable() && binding.is_pseudo_public() {
                let imported_binding = self.import(binding, directive);
                let _ = self.try_define(directive.parent, name, ns, imported_binding);
            }
        }

        // Record the destination of this import
        if let Some(did) = module.def_id() {
            let resolution = PathResolution::new(Def::Mod(did));
            self.def_map.insert(directive.id, resolution);
        }
    }

    // Miscellaneous post-processing, including recording reexports, reporting conflicts,
    // reporting the PRIVATE_IN_PUBLIC lint, and reporting unresolved imports.
    fn finalize_resolutions_in(&mut self, module: Module<'b>) {
        // Since import resolution is finished, globs will not define any more names.
        *module.globs.borrow_mut() = Vec::new();

        let mut reexports = Vec::new();
        for (&(name, ns), resolution) in module.resolutions.borrow().iter() {
            let resolution = resolution.borrow();
            let binding = match resolution.binding {
                Some(binding) => binding,
                None => continue,
            };

            // Report conflicts
            for duplicate_glob in resolution.duplicate_globs.iter() {
                // FIXME #31337: We currently allow items to shadow glob-imported re-exports.
                if !binding.is_import() {
                    if let NameBindingKind::Import { binding, .. } = duplicate_glob.kind {
                        if binding.is_import() { continue }
                    }
                }

                self.report_conflict(module, name, ns, duplicate_glob, binding);
            }

            if binding.vis == ty::Visibility::Public &&
               (binding.is_import() || binding.is_extern_crate()) {
                if let Some(def) = binding.def() {
                    reexports.push(Export { name: name, def_id: def.def_id() });
                }
            }

            if let NameBindingKind::Import { binding: orig_binding, directive, .. } = binding.kind {
                if ns == TypeNS && orig_binding.is_variant() &&
                   !orig_binding.vis.is_at_least(binding.vis, self) {
                    let msg = format!("variant `{}` is private, and cannot be reexported \
                                       (error E0364), consider declaring its enum as `pub`",
                                      name);
                    self.session.add_lint(PRIVATE_IN_PUBLIC, directive.id, binding.span, msg);
                }
            }
        }

        if reexports.len() > 0 {
            if let Some(def_id) = module.def_id() {
                let node_id = self.definitions.as_local_node_id(def_id).unwrap();
                self.export_map.insert(node_id, reexports);
            }
        }
    }
}

fn import_path_to_string(names: &[Name], subclass: &ImportDirectiveSubclass) -> String {
    if names.is_empty() {
        import_directive_subclass_to_string(subclass)
    } else {
        (format!("{}::{}",
                 names_to_string(names),
                 import_directive_subclass_to_string(subclass)))
            .to_string()
    }
}

fn import_directive_subclass_to_string(subclass: &ImportDirectiveSubclass) -> String {
    match *subclass {
        SingleImport { source, .. } => source.to_string(),
        GlobImport { .. } => "*".to_string(),
    }
}
