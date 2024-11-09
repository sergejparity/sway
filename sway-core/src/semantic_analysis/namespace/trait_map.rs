use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

use sway_error::{
    error::CompileError,
    handler::{ErrorEmitted, Handler},
};
use sway_types::{integer_bits::IntegerBits, BaseIdent, Ident, Span, Spanned};

use crate::{
    decl_engine::{
        parsed_id::ParsedDeclId, DeclEngineGet, DeclEngineGetParsedDeclId, DeclEngineInsert,
    },
    engine_threading::*,
    language::{
        parsed::{EnumDeclaration, ImplItem, StructDeclaration},
        ty::{self, TyDecl, TyImplItem, TyTraitItem},
        CallPath,
    },
    type_system::{SubstTypes, TypeId},
    IncludeSelf, SubstTypesContext, TraitConstraint, TypeArgument, TypeEngine, TypeInfo,
    TypeParameter, TypeSubstMap, UnifyCheck,
};

use super::Module;

/// Enum used to pass a value asking for insertion of type into trait map when an implementation
/// of the trait cannot be found.
#[derive(Debug, Clone)]
pub enum TryInsertingTraitImplOnFailure {
    Yes,
    No,
}

#[derive(Clone)]
pub enum CodeBlockFirstPass {
    Yes,
    No,
}

impl From<bool> for CodeBlockFirstPass {
    fn from(value: bool) -> Self {
        if value {
            CodeBlockFirstPass::Yes
        } else {
            CodeBlockFirstPass::No
        }
    }
}

#[derive(Clone, Debug)]
struct TraitSuffix {
    name: Ident,
    args: Vec<TypeArgument>,
}
impl PartialEqWithEngines for TraitSuffix {
    fn eq(&self, other: &Self, ctx: &PartialEqWithEnginesContext) -> bool {
        self.name == other.name && self.args.eq(&other.args, ctx)
    }
}
impl OrdWithEngines for TraitSuffix {
    fn cmp(&self, other: &Self, ctx: &OrdWithEnginesContext) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| self.args.cmp(&other.args, ctx))
    }
}

impl DisplayWithEngines for TraitSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>, engines: &Engines) -> fmt::Result {
        let res = write!(f, "{}", self.name.as_str());
        if !self.args.is_empty() {
            write!(
                f,
                "<{}>",
                self.args
                    .iter()
                    .map(|i| engines.help_out(i.type_id).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            res
        }
    }
}

impl DebugWithEngines for TraitSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>, engines: &Engines) -> fmt::Result {
        write!(f, "{}", engines.help_out(self))
    }
}

type TraitName = Arc<CallPath<TraitSuffix>>;

#[derive(Clone, Debug)]
struct TraitKey {
    name: TraitName,
    type_id: TypeId,
    type_id_type_parameters: Vec<TypeParameter>,
    trait_decl_span: Option<Span>,
}

impl OrdWithEngines for TraitKey {
    fn cmp(&self, other: &Self, ctx: &OrdWithEnginesContext) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name, ctx)
            .then_with(|| self.type_id.cmp(&other.type_id))
            .then_with(|| {
                self.type_id_type_parameters
                    .cmp(&other.type_id_type_parameters, ctx)
            })
    }
}

#[derive(Clone, Debug)]
pub enum ResolvedTraitImplItem {
    Parsed(ImplItem),
    Typed(TyImplItem),
}

impl DebugWithEngines for ResolvedTraitImplItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>, engines: &Engines) -> fmt::Result {
        match self {
            ResolvedTraitImplItem::Parsed(_) => panic!(),
            ResolvedTraitImplItem::Typed(ty) => write!(f, "{:?}", engines.help_out(ty)),
        }
    }
}

impl ResolvedTraitImplItem {
    fn expect_typed(self) -> TyImplItem {
        match self {
            ResolvedTraitImplItem::Parsed(_) => panic!(),
            ResolvedTraitImplItem::Typed(ty) => ty,
        }
    }

    pub fn span(&self, engines: &Engines) -> Span {
        match self {
            ResolvedTraitImplItem::Parsed(item) => item.span(engines),
            ResolvedTraitImplItem::Typed(item) => item.span(),
        }
    }
}

/// Map of name to [ResolvedTraitImplItem](ResolvedTraitImplItem)
type TraitItems = HashMap<String, ResolvedTraitImplItem>;

#[derive(Clone, Debug)]
struct TraitValue {
    trait_items: TraitItems,
    /// The span of the entire impl block.
    impl_span: Span,
}

#[derive(Clone, Debug)]
struct TraitEntry {
    key: TraitKey,
    value: TraitValue,
}

/// Map of string of type entry id and vec of [TraitEntry].
/// We are using the HashMap as a wrapper to the vec so the TraitMap algorithms
/// don't need to traverse every TraitEntry.
type TraitImpls = HashMap<TypeRootFilter, Vec<TraitEntry>>;

#[derive(Clone, Hash, Eq, PartialOrd, Ord, PartialEq, Debug)]
enum TypeRootFilter {
    Unknown,
    Never,
    Placeholder,
    TypeParam(usize),
    StringSlice,
    StringArray(usize),
    U8,
    U16,
    U32,
    U64,
    U256,
    Bool,
    Custom(String),
    B256,
    Contract,
    ErrorRecovery,
    Tuple(usize),
    Enum(ParsedDeclId<EnumDeclaration>),
    Struct(ParsedDeclId<StructDeclaration>),
    ContractCaller(String),
    Array(usize),
    RawUntypedPtr,
    RawUntypedSlice,
    Ptr,
    Slice,
    TraitType(String),
}

/// Map holding trait implementations for types.
///
/// Note: "impl self" blocks are considered traits and are stored in the
/// [TraitMap].
#[derive(Clone, Debug, Default)]
pub struct TraitMap {
    trait_impls: TraitImpls,
    satisfied_cache: HashSet<u64>,
    insert_for_type_cache: HashSet<TypeId>,
}

pub(crate) enum IsImplSelf {
    Yes,
    No,
}

pub(crate) enum IsExtendingExistingImpl {
    Yes,
    No,
}

impl TraitMap {
    /// Given a [TraitName] `trait_name`, [TypeId] `type_id`, and list of
    /// [TyImplItem](ty::TyImplItem) `items`, inserts
    /// `items` into the [TraitMap] with the key `(trait_name, type_id)`.
    ///
    /// This method is as conscious as possible of existing entries in the
    /// [TraitMap], and tries to append `items` to an existing list of
    /// declarations for the key `(trait_name, type_id)` whenever possible.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert(
        handler: &Handler,
        module: &mut Module,
        trait_name: CallPath,
        trait_type_args: Vec<TypeArgument>,
        impl_type_parameters: Vec<TypeParameter>,
        type_id: TypeId,
        items: &[ResolvedTraitImplItem],
        impl_span: &Span,
        trait_decl_span: Option<Span>,
        is_impl_self: IsImplSelf,
        is_extending_existing_impl: IsExtendingExistingImpl,
        engines: &Engines,
    ) -> Result<(), ErrorEmitted> {
        let type_id = engines.te().get_unaliased_type_id(type_id);
        let trait_map = &mut module.current_items_mut().implemented_traits;

        let mut type_id_type_parameters = match &*engines.te().get(type_id) {
            TypeInfo::Enum(decl_id) => engines.de().get_enum(decl_id).type_parameters.clone(),
            TypeInfo::Struct(decl_id) => engines.de().get_struct(decl_id).type_parameters.clone(),
            _ => vec![],
        };

        // Copy impl type parameter trait constraint to type_id_type_parameters
        for type_id_type_parameter in type_id_type_parameters.iter_mut() {
            let impl_type_parameter = impl_type_parameters
                .iter()
                .filter(|t| t.type_id == type_id_type_parameter.type_id)
                .last();
            if let Some(impl_type_parameter) = impl_type_parameter {
                type_id_type_parameter.trait_constraints =
                    impl_type_parameter.trait_constraints.clone();
            }
        }

        handler.scope(|handler| {
            let mut trait_items: TraitItems = HashMap::new();
            for item in items.iter() {
                match item {
                    ResolvedTraitImplItem::Parsed(_) => todo!(),
                    ResolvedTraitImplItem::Typed(ty_item) => match ty_item {
                        TyImplItem::Fn(decl_ref) => {
                            if trait_items
                                .insert(decl_ref.name().clone().to_string(), item.clone())
                                .is_some()
                            {
                                // duplicate method name
                                handler.emit_err(CompileError::MultipleDefinitionsOfName {
                                    name: decl_ref.name().clone(),
                                    span: decl_ref.span(),
                                });
                            }
                        }
                        TyImplItem::Constant(decl_ref) => {
                            trait_items.insert(decl_ref.name().to_string(), item.clone());
                        }
                        TyImplItem::Type(decl_ref) => {
                            trait_items.insert(decl_ref.name().to_string(), item.clone());
                        }
                    },
                }
            }

            let trait_impls = trait_map.get_impls_mut(engines, type_id).clone();

            // check to see if adding this trait will produce a conflicting definition
            for TraitEntry {
                key:
                    TraitKey {
                        name: map_trait_name,
                        type_id: map_type_id,
                        type_id_type_parameters: map_type_id_type_parameters,
                        trait_decl_span: _,
                    },
                value:
                    TraitValue {
                        trait_items: map_trait_items,
                        impl_span: existing_impl_span,
                    },
            } in trait_impls.iter()
            {
                let CallPath {
                    suffix:
                        TraitSuffix {
                            name: map_trait_name_suffix,
                            args: map_trait_type_args,
                        },
                    ..
                } = &*map_trait_name.clone();

                let unify_checker = UnifyCheck::non_generic_constraint_subset(engines);

                // Types are subset if the `type_id` that we want to insert can unify with the
                // existing `map_type_id`. In addition we need to additionally check for the case of
                // `&mut <type>` and `&<type>`.
                let types_are_subset = unify_checker.check(type_id, *map_type_id)
                    && is_unified_type_subset(engines.te(), type_id, *map_type_id);

                /// `left` can unify into `right`. Additionally we need to check subset condition in case of
                /// [TypeInfo::Ref] types.  Although `&mut <type>` can unify with `&<type>`
                /// when it comes to trait and self impls, we considered them to be different types.
                /// E.g., we can have `impl Foo for &T` and at the same time `impl Foo for &mut T`.
                /// Or in general, `impl Foo for & &mut .. &T` is different type then, e.g., `impl Foo for &mut & .. &mut T`.
                fn is_unified_type_subset(
                    type_engine: &TypeEngine,
                    mut left: TypeId,
                    mut right: TypeId,
                ) -> bool {
                    // The loop cannot be endless, because at the end we must hit a referenced type which is not
                    // a reference.
                    loop {
                        let left_ty_info = &*type_engine.get_unaliased(left);
                        let right_ty_info = &*type_engine.get_unaliased(right);
                        match (left_ty_info, right_ty_info) {
                            (
                                TypeInfo::Ref {
                                    to_mutable_value: l_to_mut,
                                    ..
                                },
                                TypeInfo::Ref {
                                    to_mutable_value: r_to_mut,
                                    ..
                                },
                            ) if *l_to_mut != *r_to_mut => return false, // Different mutability means not subset.
                            (
                                TypeInfo::Ref {
                                    referenced_type: l_ty,
                                    ..
                                },
                                TypeInfo::Ref {
                                    referenced_type: r_ty,
                                    ..
                                },
                            ) => {
                                left = l_ty.type_id;
                                right = r_ty.type_id;
                            }
                            _ => return true,
                        }
                    }
                }

                let mut traits_are_subset = true;
                if *map_trait_name_suffix != trait_name.suffix
                    || map_trait_type_args.len() != trait_type_args.len()
                {
                    traits_are_subset = false;
                } else {
                    for (map_arg_type, arg_type) in
                        map_trait_type_args.iter().zip(trait_type_args.iter())
                    {
                        if !unify_checker.check(arg_type.type_id, map_arg_type.type_id) {
                            traits_are_subset = false;
                        }
                    }
                }

                let mut trait_constraints_safified = true;
                for (map_type_id_type_parameter, type_id_type_parameter) in
                    map_type_id_type_parameters
                        .iter()
                        .zip(type_id_type_parameters.iter())
                {
                    // Check that type_id_type_parameter satisfies all trait constraints in map_type_id_type_parameter.
                    if type_id_type_parameter
                        .type_id
                        .is_concrete(engines, crate::TreatNumericAs::Abstract)
                        && !TraitMap::check_if_trait_constraints_are_satisfied_for_type(
                            &Handler::default(),
                            module,
                            type_id_type_parameter.type_id,
                            &map_type_id_type_parameter.trait_constraints,
                            impl_span,
                            engines,
                        )
                        .is_ok()
                    {
                        trait_constraints_safified = false;
                    }
                }

                if !trait_constraints_safified {
                    continue;
                }

                if matches!(is_extending_existing_impl, IsExtendingExistingImpl::No)
                    && types_are_subset
                    && traits_are_subset
                    && matches!(is_impl_self, IsImplSelf::No)
                {
                    handler.emit_err(CompileError::ConflictingImplsForTraitAndType {
                        trait_name: trait_name.to_string_with_args(engines, &trait_type_args),
                        type_implementing_for: engines.help_out(type_id).to_string(),
                        existing_impl_span: existing_impl_span.clone(),
                        second_impl_span: impl_span.clone(),
                    });
                } else if types_are_subset
                    && (traits_are_subset || matches!(is_impl_self, IsImplSelf::Yes))
                {
                    let mut names = trait_items.keys().clone().collect::<Vec<_>>();
                    names.sort();
                    for name in names {
                        let item = &trait_items[name];
                        match item {
                            ResolvedTraitImplItem::Parsed(_item) => todo!(),
                            ResolvedTraitImplItem::Typed(item) => match item {
                                ty::TyTraitItem::Fn(decl_ref) => {
                                    if map_trait_items.get(name).is_some() {
                                        handler.emit_err(
                                            CompileError::DuplicateDeclDefinedForType {
                                                decl_kind: "method".into(),
                                                decl_name: decl_ref.name().to_string(),
                                                type_implementing_for: engines
                                                    .help_out(type_id)
                                                    .to_string(),
                                                span: decl_ref.name().span(),
                                            },
                                        );
                                    }
                                }
                                ty::TyTraitItem::Constant(decl_ref) => {
                                    if map_trait_items.get(name).is_some() {
                                        handler.emit_err(
                                            CompileError::DuplicateDeclDefinedForType {
                                                decl_kind: "constant".into(),
                                                decl_name: decl_ref.name().to_string(),
                                                type_implementing_for: engines
                                                    .help_out(type_id)
                                                    .to_string(),
                                                span: decl_ref.name().span(),
                                            },
                                        );
                                    }
                                }
                                ty::TyTraitItem::Type(decl_ref) => {
                                    if map_trait_items.get(name).is_some() {
                                        handler.emit_err(
                                            CompileError::DuplicateDeclDefinedForType {
                                                decl_kind: "type".into(),
                                                decl_name: decl_ref.name().to_string(),
                                                type_implementing_for: engines
                                                    .help_out(type_id)
                                                    .to_string(),
                                                span: decl_ref.name().span(),
                                            },
                                        );
                                    }
                                }
                            },
                        }
                    }
                }
            }
            let trait_name: TraitName = Arc::new(CallPath {
                prefixes: trait_name.prefixes,
                suffix: TraitSuffix {
                    name: trait_name.suffix,
                    args: trait_type_args,
                },
                callpath_type: trait_name.callpath_type,
            });

            // even if there is a conflicting definition, add the trait anyway
            trait_map.insert_inner(
                trait_name,
                impl_span.clone(),
                trait_decl_span,
                type_id,
                type_id_type_parameters,
                trait_items,
                engines,
            );

            Ok(())
        })
    }

    fn insert_inner(
        &mut self,
        trait_name: TraitName,
        impl_span: Span,
        trait_decl_span: Option<Span>,
        type_id: TypeId,
        type_id_type_parameters: Vec<TypeParameter>,
        trait_methods: TraitItems,
        engines: &Engines,
    ) {
        let key = TraitKey {
            name: trait_name,
            type_id,
            type_id_type_parameters,
            trait_decl_span,
        };
        let value = TraitValue {
            trait_items: trait_methods,
            impl_span,
        };
        let entry = TraitEntry { key, value };
        let mut trait_impls: TraitImpls = HashMap::<TypeRootFilter, Vec<TraitEntry>>::new();
        let type_root_filter = Self::get_type_root_filter(engines, type_id);
        let impls_vector = vec![entry];
        trait_impls.insert(type_root_filter, impls_vector);

        let trait_map = TraitMap {
            trait_impls,
            satisfied_cache: HashSet::default(),
        };

        self.extend(trait_map, engines);
    }

    /// Given [TraitMap]s `self` and `other`, extend `self` with `other`,
    /// extending existing entries when possible.
    pub(crate) fn extend(&mut self, other: TraitMap, engines: &Engines) {
        let mut impls_keys = other.trait_impls.keys().clone().collect::<Vec<_>>();
        impls_keys.sort();
        for impls_key in impls_keys {
            let oe_vec = &other.trait_impls[impls_key];
            let self_vec = if let Some(self_vec) = self.trait_impls.get_mut(impls_key) {
                self_vec
            } else {
                self.trait_impls
                    .insert(impls_key.clone(), Vec::<TraitEntry>::new());
                self.trait_impls.get_mut(impls_key).unwrap()
            };

            for oe in oe_vec.iter() {
                let pos = self_vec.binary_search_by(|se| {
                    se.key.cmp(&oe.key, &OrdWithEnginesContext::new(engines))
                });

                match pos {
                    Ok(pos) => self_vec[pos]
                        .value
                        .trait_items
                        .extend(oe.value.trait_items.clone()),
                    Err(pos) => self_vec.insert(pos, oe.clone()),
                }
            }
        }
    }

    /// Filters the entries in `self` and return a new [TraitMap] with all of
    /// the entries from `self` that implement a trait from the declaration with that span.
    pub(crate) fn filter_by_trait_decl_span(&self, trait_decl_span: Span) -> TraitMap {
        let mut trait_map = TraitMap::default();
        let mut keys = self.trait_impls.keys().clone().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            let vec = &self.trait_impls[key];
            for entry in vec {
                if entry.key.trait_decl_span.as_ref() == Some(&trait_decl_span) {
                    let trait_map_vec =
                        if let Some(trait_map_vec) = trait_map.trait_impls.get_mut(key) {
                            trait_map_vec
                        } else {
                            trait_map
                                .trait_impls
                                .insert(key.clone(), Vec::<TraitEntry>::new());
                            trait_map.trait_impls.get_mut(key).unwrap()
                        };

                    trait_map_vec.push(entry.clone());
                }
            }
        }
        trait_map
    }

    /// Filters the entries in `self` with the given [TypeId] `type_id` and
    /// return a new [TraitMap] with all of the entries from `self` for which
    /// `type_id` is a subtype or a supertype. Additionally, the new [TraitMap]
    /// contains the entries for the inner types of `self`.
    ///
    /// This is used for handling the case in which we need to import an impl
    /// block from another module, and the type that that impl block is defined
    /// for is of the type that we are importing, but in a more concrete form.
    ///
    /// Here is some example Sway code that we should expect to compile:
    ///
    /// `my_double.sw`:
    /// ```ignore
    /// library;
    ///
    /// pub trait MyDouble<T> {
    ///     fn my_double(self, input: T) -> T;
    /// }
    /// ```
    ///
    /// `my_point.sw`:
    /// ```ignore
    /// library;
    ///
    /// use ::my_double::MyDouble;
    ///
    /// pub struct MyPoint<T> {
    ///     x: T,
    ///     y: T,
    /// }
    ///
    /// impl MyDouble<u64> for MyPoint<u64> {
    ///     fn my_double(self, value: u64) -> u64 {
    ///         (self.x*2) + (self.y*2) + (value*2)
    ///     }
    /// }
    /// ```
    ///
    /// `main.sw`:
    /// ```ignore
    /// script;
    ///
    /// mod my_double;
    /// mod my_point;
    ///
    /// use my_point::MyPoint;
    ///
    /// fn main() -> u64 {
    ///     let foo = MyPoint {
    ///         x: 10u64,
    ///         y: 10u64,
    ///     };
    ///     foo.my_double(100)
    /// }
    /// ```
    ///
    /// We need to be able to import the trait defined upon `MyPoint<u64>` just
    /// from seeing `use ::my_double::MyDouble;`.
    pub(crate) fn filter_by_type_item_import(
        &self,
        type_id: TypeId,
        engines: &Engines,
    ) -> TraitMap {
        let unify_checker = UnifyCheck::constraint_subset(engines);
        let unify_checker_for_item_import = UnifyCheck::non_generic_constraint_subset(engines);

        // a curried version of the decider protocol to use in the helper functions
        let decider = |left: TypeId, right: TypeId| {
            unify_checker.check(left, right) || unify_checker_for_item_import.check(right, left)
        };
        let mut trait_map = self.filter_by_type_inner(engines, vec![type_id], decider);
        let all_types = type_id
            .extract_inner_types(engines, IncludeSelf::No)
            .into_iter()
            .collect::<Vec<_>>();
        // a curried version of the decider protocol to use in the helper functions
        let decider2 = |left: TypeId, right: TypeId| unify_checker.check(left, right);

        trait_map.extend(
            self.filter_by_type_inner(engines, all_types, decider2),
            engines,
        );
        trait_map
    }

    fn filter_by_type_inner(
        &self,
        engines: &Engines,
        mut all_types: Vec<TypeId>,
        decider: impl Fn(TypeId, TypeId) -> bool,
    ) -> TraitMap {
        let type_engine = engines.te();
        let mut trait_map = TraitMap::default();
        for type_id in all_types.iter_mut() {
            let type_info = type_engine.get(*type_id);
            let impls = self.get_impls(engines, *type_id, true);
            for TraitEntry {
                key:
                    TraitKey {
                        name: map_trait_name,
                        type_id: map_type_id,
                        type_id_type_parameters: map_type_id_constraints,
                        trait_decl_span: map_trait_decl_span,
                    },
                value:
                    TraitValue {
                        trait_items: map_trait_items,
                        impl_span,
                    },
            } in impls.iter()
            {
                if !type_engine.is_type_changeable(engines, &type_info) && *type_id == *map_type_id
                {
                    trait_map.insert_inner(
                        map_trait_name.clone(),
                        impl_span.clone(),
                        map_trait_decl_span.clone(),
                        *type_id,
                        map_type_id_constraints.clone(),
                        map_trait_items.clone(),
                        engines,
                    );
                } else if decider(*type_id, *map_type_id) {
                    trait_map.insert_inner(
                        map_trait_name.clone(),
                        impl_span.clone(),
                        map_trait_decl_span.clone(),
                        *map_type_id,
                        map_type_id_constraints.clone(),
                        Self::filter_dummy_methods(
                            map_trait_items.clone(),
                            *type_id,
                            *map_type_id,
                            engines,
                        ),
                        engines,
                    );
                }
            }
        }
        trait_map
    }

    fn filter_dummy_methods(
        map_trait_items: TraitItems,
        type_id: TypeId,
        map_type_id: TypeId,
        engines: &Engines,
    ) -> TraitItems {
        let mut insertable = true;
        if let TypeInfo::UnknownGeneric {
            is_from_type_parameter,
            ..
        } = *engines.te().get(map_type_id)
        {
            insertable = !is_from_type_parameter
                || matches!(*engines.te().get(type_id), TypeInfo::UnknownGeneric { .. });
        }

        map_trait_items
            .clone()
            .into_iter()
            .filter_map(|(name, item)| match item {
                ResolvedTraitImplItem::Parsed(_item) => todo!(),
                ResolvedTraitImplItem::Typed(item) => match item {
                    ty::TyTraitItem::Fn(decl_ref) => {
                        let decl = (*engines.de().get(decl_ref.id())).clone();
                        if decl.is_trait_method_dummy && !insertable {
                            None
                        } else {
                            Some((name, ResolvedTraitImplItem::Typed(TyImplItem::Fn(decl_ref))))
                        }
                    }
                    ty::TyTraitItem::Constant(decl_ref) => Some((
                        name,
                        ResolvedTraitImplItem::Typed(TyImplItem::Constant(decl_ref)),
                    )),
                    ty::TyTraitItem::Type(decl_ref) => Some((
                        name,
                        ResolvedTraitImplItem::Typed(TyImplItem::Type(decl_ref)),
                    )),
                },
            })
            .collect()
    }

    fn make_item_for_type_mapping(
        engines: &Engines,
        item: ResolvedTraitImplItem,
        mut type_mapping: TypeSubstMap,
        type_id: TypeId,
        code_block_first_pass: CodeBlockFirstPass,
    ) -> ResolvedTraitImplItem {
        let decl_engine = engines.de();
        match &item {
            ResolvedTraitImplItem::Parsed(_item) => todo!(),
            ResolvedTraitImplItem::Typed(item) => match item {
                ty::TyTraitItem::Fn(decl_ref) => {
                    let mut decl = (*decl_engine.get(decl_ref.id())).clone();
                    if let Some(decl_implementing_for_typeid) = decl.implementing_for_typeid {
                        type_mapping.insert(decl_implementing_for_typeid, type_id);
                    }
                    decl.subst(&SubstTypesContext::new(
                        engines,
                        &type_mapping,
                        matches!(code_block_first_pass, CodeBlockFirstPass::No),
                    ));
                    let new_ref = decl_engine
                        .insert(decl, decl_engine.get_parsed_decl_id(decl_ref.id()).as_ref())
                        .with_parent(decl_engine, decl_ref.id().into());

                    ResolvedTraitImplItem::Typed(TyImplItem::Fn(new_ref))
                }
                ty::TyTraitItem::Constant(decl_ref) => {
                    let mut decl = (*decl_engine.get(decl_ref.id())).clone();
                    decl.subst(&SubstTypesContext::new(
                        engines,
                        &type_mapping,
                        matches!(code_block_first_pass, CodeBlockFirstPass::No),
                    ));
                    let new_ref = decl_engine
                        .insert(decl, decl_engine.get_parsed_decl_id(decl_ref.id()).as_ref());
                    ResolvedTraitImplItem::Typed(TyImplItem::Constant(new_ref))
                }
                ty::TyTraitItem::Type(decl_ref) => {
                    let mut decl = (*decl_engine.get(decl_ref.id())).clone();
                    decl.subst(&SubstTypesContext::new(
                        engines,
                        &type_mapping,
                        matches!(code_block_first_pass, CodeBlockFirstPass::No),
                    ));
                    let new_ref = decl_engine
                        .insert(decl, decl_engine.get_parsed_decl_id(decl_ref.id()).as_ref());
                    ResolvedTraitImplItem::Typed(TyImplItem::Type(new_ref))
                }
            },
        }
    }

    /// Find the entries in `self` that are equivalent to `type_id`.
    ///
    /// Notes:
    /// - equivalency is defined (1) based on whether the types contains types
    ///     that are dynamic and can change and (2) whether the types hold
    ///     equivalency after (1) is fulfilled
    /// - this method does not translate types from the found entries to the
    ///     `type_id` (like in `filter_by_type()`). This is because the only
    ///     entries that qualify as hits are equivalents of `type_id`
    pub(crate) fn get_items_for_type(
        module: &Module,
        engines: &Engines,
        type_id: TypeId,
    ) -> Vec<ResolvedTraitImplItem> {
        TraitMap::get_items_and_trait_key_for_type(module, engines, type_id)
            .iter()
            .map(|i| i.0.clone())
            .collect::<Vec<_>>()
    }

    fn get_items_and_trait_key_for_type(
        module: &Module,
        engines: &Engines,
        type_id: TypeId,
    ) -> Vec<(ResolvedTraitImplItem, TraitKey)> {
        let type_engine = engines.te();
        let unify_check = UnifyCheck::constraint_subset(engines);

        let type_id = engines.te().get_unaliased_type_id(type_id);

        let mut items = vec![];
        // small performance gain in bad case
        if matches!(&*type_engine.get(type_id), TypeInfo::ErrorRecovery(_)) {
            return items;
        }

        let _ = module.walk_scope_chain(|lexical_scope| {
            let impls = lexical_scope
                .items
                .implemented_traits
                .get_impls(engines, type_id, true);
            for entry in impls {
                if unify_check.check(type_id, entry.key.type_id) {
                    let trait_items = Self::filter_dummy_methods(
                        entry.value.trait_items,
                        type_id,
                        entry.key.type_id,
                        engines,
                    )
                    .values()
                    .cloned()
                    .map(|i| (i, entry.key.clone()))
                    .collect::<Vec<_>>();

                    items.extend(trait_items);
                }
            }
            Ok(None::<()>)
        });
        items
    }

    /// Find the spans of all impls for the given type.
    ///
    /// Notes:
    /// - equivalency is defined (1) based on whether the types contains types
    ///     that are dynamic and can change and (2) whether the types hold
    ///     equivalency after (1) is fulfilled
    /// - this method does not translate types from the found entries to the
    ///     `type_id` (like in `filter_by_type()`). This is because the only
    ///     entries that qualify as hits are equivalents of `type_id`
    pub fn get_impl_spans_for_type(
        module: &Module,
        engines: &Engines,
        type_id: &TypeId,
    ) -> Vec<Span> {
        let type_engine = engines.te();
        let unify_check = UnifyCheck::constraint_subset(engines);

        let type_id = &engines.te().get_unaliased_type_id(*type_id);

        let mut spans = vec![];
        // small performance gain in bad case
        if matches!(&*type_engine.get(*type_id), TypeInfo::ErrorRecovery(_)) {
            return spans;
        }
        let _ = module.walk_scope_chain(|lexical_scope| {
            let impls = lexical_scope
                .items
                .implemented_traits
                .get_impls(engines, *type_id, false);
            for entry in impls {
                if unify_check.check(*type_id, entry.key.type_id) {
                    spans.push(entry.value.impl_span.clone());
                }
            }
            Ok(None::<()>)
        });

        spans
    }

    /// Find the spans of all impls for the given decl.
    pub fn get_impl_spans_for_decl(
        module: &Module,
        engines: &Engines,
        ty_decl: &TyDecl,
    ) -> Vec<Span> {
        let handler = Handler::default();
        ty_decl
            .return_type(&handler, engines)
            .map(|type_id| TraitMap::get_impl_spans_for_type(module, engines, &type_id))
            .unwrap_or_default()
    }

    /// Find the entries in `self` with trait name `trait_name` and return the
    /// spans of the impls.
    pub fn get_impl_spans_for_trait_name(module: &Module, trait_name: &CallPath) -> Vec<Span> {
        let mut spans = vec![];
        let _ = module.walk_scope_chain(|lexical_scope| {
            spans.push(
                lexical_scope
                    .items
                    .implemented_traits
                    .trait_impls
                    .values()
                    .map(|impls| {
                        impls
                            .iter()
                            .filter_map(|entry| {
                                let map_trait_name = CallPath {
                                    prefixes: entry.key.name.prefixes.clone(),
                                    suffix: entry.key.name.suffix.name.clone(),
                                    callpath_type: entry.key.name.callpath_type,
                                };
                                if &map_trait_name == trait_name {
                                    Some(entry.value.impl_span.clone())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<Span>>()
                    })
                    .collect::<Vec<Vec<Span>>>()
                    .concat(),
            );
            Ok(None::<()>)
        });

        spans.concat()
    }

    /// Find the entries in `self` that are equivalent to `type_id` with trait
    /// name `trait_name` and with trait type arguments.
    ///
    /// Notes:
    /// - equivalency is defined (1) based on whether the types contains types
    ///     that are dynamic and can change and (2) whether the types hold
    ///     equivalency after (1) is fulfilled
    /// - this method does not translate types from the found entries to the
    ///     `type_id` (like in `filter_by_type()`). This is because the only
    ///     entries that qualify as hits are equivalents of `type_id`
    pub(crate) fn get_items_for_type_and_trait_name_and_trait_type_arguments(
        module: &Module,
        engines: &Engines,
        type_id: TypeId,
        trait_name: &CallPath,
        trait_type_args: &[TypeArgument],
    ) -> Vec<ResolvedTraitImplItem> {
        let type_id = engines.te().get_unaliased_type_id(type_id);

        let type_engine = engines.te();
        let unify_check = UnifyCheck::constraint_subset(engines);
        let mut items = vec![];
        // small performance gain in bad case
        if matches!(&*type_engine.get(type_id), TypeInfo::ErrorRecovery(_)) {
            return items;
        }
        let _ = module.walk_scope_chain(|lexical_scope| {
            let impls = lexical_scope
                .items
                .implemented_traits
                .get_impls(engines, type_id, false);
            for e in impls {
                let map_trait_name = CallPath {
                    prefixes: e.key.name.prefixes.clone(),
                    suffix: e.key.name.suffix.name.clone(),
                    callpath_type: e.key.name.callpath_type,
                };
                if &map_trait_name == trait_name
                    && unify_check.check(type_id, e.key.type_id)
                    && trait_type_args.len() == e.key.name.suffix.args.len()
                    && trait_type_args
                        .iter()
                        .zip(e.key.name.suffix.args.iter())
                        .all(|(t1, t2)| unify_check.check(t1.type_id, t2.type_id))
                {
                    let type_mapping = TypeSubstMap::from_superset_and_subset(
                        engines.te(),
                        engines.de(),
                        e.key.type_id,
                        type_id,
                    );

                    let mut trait_items = Self::filter_dummy_methods(
                        e.value.trait_items,
                        type_id,
                        e.key.type_id,
                        engines,
                    )
                    .values()
                    .cloned()
                    .map(|i| {
                        Self::make_item_for_type_mapping(
                            engines,
                            i,
                            type_mapping.clone(),
                            type_id,
                            CodeBlockFirstPass::No,
                        )
                    })
                    .collect::<Vec<_>>();

                    items.append(&mut trait_items);
                }
            }
            Ok(None::<()>)
        });
        items
    }

    /// Find the entries in `self` that are equivalent to `type_id` with trait
    /// name `trait_name` and with trait type arguments.
    ///
    /// Notes:
    /// - equivalency is defined (1) based on whether the types contains types
    ///     that are dynamic and can change and (2) whether the types hold
    ///     equivalency after (1) is fulfilled
    /// - this method does not translate types from the found entries to the
    ///     `type_id` (like in `filter_by_type()`). This is because the only
    ///     entries that qualify as hits are equivalents of `type_id`
    pub(crate) fn get_items_for_type_and_trait_name_and_trait_type_arguments_typed(
        module: &Module,
        engines: &Engines,
        type_id: TypeId,
        trait_name: &CallPath,
        trait_type_args: &[TypeArgument],
    ) -> Vec<ty::TyTraitItem> {
        TraitMap::get_items_for_type_and_trait_name_and_trait_type_arguments(
            module,
            engines,
            type_id,
            trait_name,
            trait_type_args,
        )
        .into_iter()
        .map(|item| item.expect_typed())
        .collect::<Vec<_>>()
    }

    pub(crate) fn get_trait_names_and_type_arguments_for_type(
        module: &Module,
        engines: &Engines,
        type_id: TypeId,
    ) -> Vec<(CallPath, Vec<TypeArgument>)> {
        let type_id = engines.te().get_unaliased_type_id(type_id);

        let type_engine = engines.te();
        let unify_check = UnifyCheck::constraint_subset(engines);
        let mut trait_names = vec![];
        // small performance gain in bad case
        if matches!(&*type_engine.get(type_id), TypeInfo::ErrorRecovery(_)) {
            return trait_names;
        }
        let _ = module.walk_scope_chain(|lexical_scope| {
            let impls = lexical_scope
                .items
                .implemented_traits
                .get_impls(engines, type_id, false);
            for entry in impls {
                if unify_check.check(type_id, entry.key.type_id) {
                    let trait_call_path = CallPath {
                        prefixes: entry.key.name.prefixes.clone(),
                        suffix: entry.key.name.suffix.name.clone(),
                        callpath_type: entry.key.name.callpath_type,
                    };
                    trait_names.push((trait_call_path, entry.key.name.suffix.args.clone()));
                }
            }
            Ok(None::<()>)
        });
        trait_names
    }

    pub(crate) fn get_trait_item_for_type(
        module: &Module,
        handler: &Handler,
        engines: &Engines,
        symbol: &Ident,
        type_id: TypeId,
        as_trait: Option<CallPath>,
    ) -> Result<ResolvedTraitImplItem, ErrorEmitted> {
        let type_id = engines.te().get_unaliased_type_id(type_id);

        let mut candidates = HashMap::<String, ResolvedTraitImplItem>::new();
        for (trait_item, trait_key) in
            TraitMap::get_items_and_trait_key_for_type(module, engines, type_id)
        {
            match trait_item {
                ResolvedTraitImplItem::Parsed(impl_item) => match impl_item {
                    ImplItem::Fn(fn_ref) => {
                        let decl = engines.pe().get_function(&fn_ref);
                        let trait_call_path_string = engines.help_out(&*trait_key.name).to_string();
                        if decl.name.as_str() == symbol.as_str()
                            && (as_trait.is_none()
                                || as_trait.clone().unwrap().to_string() == trait_call_path_string)
                        {
                            candidates.insert(
                                trait_call_path_string,
                                ResolvedTraitImplItem::Parsed(ImplItem::Fn(fn_ref)),
                            );
                        }
                    }
                    ImplItem::Constant(const_ref) => {
                        let decl = engines.pe().get_constant(&const_ref);
                        let trait_call_path_string = engines.help_out(&*trait_key.name).to_string();
                        if decl.name.as_str() == symbol.as_str()
                            && (as_trait.is_none()
                                || as_trait.clone().unwrap().to_string() == trait_call_path_string)
                        {
                            candidates.insert(
                                trait_call_path_string,
                                ResolvedTraitImplItem::Parsed(ImplItem::Constant(const_ref)),
                            );
                        }
                    }
                    ImplItem::Type(type_ref) => {
                        let decl = engines.pe().get_trait_type(&type_ref);
                        let trait_call_path_string = engines.help_out(&*trait_key.name).to_string();
                        if decl.name.as_str() == symbol.as_str()
                            && (as_trait.is_none()
                                || as_trait.clone().unwrap().to_string() == trait_call_path_string)
                        {
                            candidates.insert(
                                trait_call_path_string,
                                ResolvedTraitImplItem::Parsed(ImplItem::Type(type_ref)),
                            );
                        }
                    }
                },
                ResolvedTraitImplItem::Typed(ty_impl_item) => match ty_impl_item {
                    ty::TyTraitItem::Fn(fn_ref) => {
                        let decl = engines.de().get_function(&fn_ref);
                        let trait_call_path_string = engines.help_out(&*trait_key.name).to_string();
                        if decl.name.as_str() == symbol.as_str()
                            && (as_trait.is_none()
                                || as_trait.clone().unwrap().to_string() == trait_call_path_string)
                        {
                            candidates.insert(
                                trait_call_path_string,
                                ResolvedTraitImplItem::Typed(TyTraitItem::Fn(fn_ref)),
                            );
                        }
                    }
                    ty::TyTraitItem::Constant(const_ref) => {
                        let decl = engines.de().get_constant(&const_ref);
                        let trait_call_path_string = engines.help_out(&*trait_key.name).to_string();
                        if decl.call_path.suffix.as_str() == symbol.as_str()
                            && (as_trait.is_none()
                                || as_trait.clone().unwrap().to_string() == trait_call_path_string)
                        {
                            candidates.insert(
                                trait_call_path_string,
                                ResolvedTraitImplItem::Typed(TyTraitItem::Constant(const_ref)),
                            );
                        }
                    }
                    ty::TyTraitItem::Type(type_ref) => {
                        let decl = engines.de().get_type(&type_ref);
                        let trait_call_path_string = engines.help_out(&*trait_key.name).to_string();
                        if decl.name.as_str() == symbol.as_str()
                            && (as_trait.is_none()
                                || as_trait.clone().unwrap().to_string() == trait_call_path_string)
                        {
                            candidates.insert(
                                trait_call_path_string,
                                ResolvedTraitImplItem::Typed(TyTraitItem::Type(type_ref)),
                            );
                        }
                    }
                },
            }
        }

        match candidates.len().cmp(&1) {
            Ordering::Greater => Err(handler.emit_err(
                CompileError::MultipleApplicableItemsInScope {
                    item_name: symbol.as_str().to_string(),
                    item_kind: "item".to_string(),
                    as_traits: candidates
                        .keys()
                        .map(|k| {
                            (
                                k.clone()
                                    .split("::")
                                    .collect::<Vec<_>>()
                                    .last()
                                    .unwrap()
                                    .to_string(),
                                engines.help_out(type_id).to_string(),
                            )
                        })
                        .collect::<Vec<_>>(),
                    item_paths: candidates
                        .values()
                        .filter_map(|i| i.span(engines).to_string_path_with_line_col(engines.se()))
                        .collect::<Vec<String>>(),
                    span: symbol.span(),
                },
            )),
            Ordering::Less => Err(handler.emit_err(CompileError::SymbolNotFound {
                name: symbol.clone(),
                span: symbol.span(),
            })),
            Ordering::Equal => Ok(candidates.values().next().unwrap().clone()),
        }
    }

    /// Checks to see if the trait constraints are satisfied for a given type.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn check_if_trait_constraints_are_satisfied_for_type(
        handler: &Handler,
        module: &mut Module,
        type_id: TypeId,
        constraints: &[TraitConstraint],
        access_span: &Span,
        engines: &Engines,
    ) -> Result<(), ErrorEmitted> {
        let type_engine = engines.te();

        let type_id = type_engine.get_unaliased_type_id(type_id);

        // resolving trait constraints require a concrete type, we need to default numeric to u64
        type_engine.decay_numeric(handler, engines, type_id, access_span)?;

        if constraints.is_empty() {
            return Ok(());
        }

        // Check we can use the cache
        let mut hasher = DefaultHasher::default();
        type_id.hash(&mut hasher);
        for c in constraints {
            c.hash(&mut hasher, engines);
        }
        let hash = hasher.finish();

        {
            let trait_map = &mut module.current_lexical_scope_mut().items.implemented_traits;
            if trait_map.satisfied_cache.contains(&hash) {
                return Ok(());
            }
        }

        let all_impld_traits: BTreeSet<(Ident, TypeId)> =
            Self::get_all_implemented_traits(module, type_id, engines);

        // Call the real implementation and cache when true
        match Self::check_if_trait_constraints_are_satisfied_for_type_inner(
            handler,
            type_id,
            constraints,
            access_span,
            engines,
            all_impld_traits,
        ) {
            Ok(()) => {
                let trait_map = &mut module.current_lexical_scope_mut().items.implemented_traits;
                trait_map.satisfied_cache.insert(hash);
                Ok(())
            }
            r => r,
        }
    }

    fn get_all_implemented_traits(
        module: &Module,
        type_id: TypeId,
        engines: &Engines,
    ) -> BTreeSet<(Ident, TypeId)> {
        let mut all_impld_traits: BTreeSet<(Ident, TypeId)> = Default::default();
        let _ = module.walk_scope_chain(|lexical_scope| {
            all_impld_traits.extend(
                lexical_scope
                    .items
                    .implemented_traits
                    .get_implemented_traits(type_id, engines),
            );
            Ok(None::<()>)
        });
        all_impld_traits
    }

    fn get_implemented_traits(
        &self,
        type_id: TypeId,
        engines: &Engines,
    ) -> BTreeSet<(Ident, TypeId)> {
        let type_engine = engines.te();
        let unify_check = UnifyCheck::constraint_subset(engines);

        let impls = self.get_impls(engines, type_id, true);
        let all_impld_traits: BTreeSet<(Ident, TypeId)> = impls
            .iter()
            .filter_map(|e| {
                let key = &e.key;
                let suffix = &key.name.suffix;
                if unify_check.check(type_id, key.type_id) {
                    let map_trait_type_id = type_engine.new_custom(
                        engines,
                        suffix.name.clone().into(),
                        if suffix.args.is_empty() {
                            None
                        } else {
                            Some(suffix.args.to_vec())
                        },
                    );
                    Some((suffix.name.clone(), map_trait_type_id))
                } else {
                    None
                }
            })
            .collect();

        all_impld_traits
    }

    #[allow(clippy::too_many_arguments)]
    fn check_if_trait_constraints_are_satisfied_for_type_inner(
        handler: &Handler,
        type_id: TypeId,
        constraints: &[TraitConstraint],
        access_span: &Span,
        engines: &Engines,
        all_impld_traits: BTreeSet<(Ident, TypeId)>,
    ) -> Result<(), ErrorEmitted> {
        let type_engine = engines.te();
        let unify_check = UnifyCheck::constraint_subset(engines);

        let required_traits: BTreeSet<(Ident, TypeId)> = constraints
            .iter()
            .map(|c| {
                let TraitConstraint {
                    trait_name: constraint_trait_name,
                    type_arguments: constraint_type_arguments,
                } = c;
                let constraint_type_id = type_engine.new_custom(
                    engines,
                    constraint_trait_name.suffix.clone().into(),
                    if constraint_type_arguments.is_empty() {
                        None
                    } else {
                        Some(constraint_type_arguments.clone())
                    },
                );
                (c.trait_name.suffix.clone(), constraint_type_id)
            })
            .collect();

        let traits_not_found: BTreeSet<(BaseIdent, TypeId)> = required_traits
            .into_iter()
            .filter(|(required_trait_name, required_trait_type_id)| {
                !all_impld_traits
                    .iter()
                    .any(|(trait_name, constraint_type_id)| {
                        trait_name == required_trait_name
                            && unify_check.check(*constraint_type_id, *required_trait_type_id)
                    })
            })
            .collect();

        handler.scope(|handler| {
            for (trait_name, constraint_type_id) in traits_not_found.iter() {
                let mut type_arguments_string = "".to_string();
                if let TypeInfo::Custom {
                    qualified_call_path: _,
                    type_arguments: Some(type_arguments),
                } = &*type_engine.get(*constraint_type_id)
                {
                    type_arguments_string = format!("<{}>", engines.help_out(type_arguments));
                }

                // TODO: use a better span
                handler.emit_err(CompileError::TraitConstraintNotSatisfied {
                    type_id: type_id.index(),
                    ty: engines.help_out(type_id).to_string(),
                    trait_name: format!("{}{}", trait_name, type_arguments_string),
                    span: access_span.clone(),
                });
            }

            Ok(())
        })
    }

    pub fn get_trait_constraints_are_satisfied_for_types(
        module: &Module,
        _handler: &Handler,
        type_id: TypeId,
        constraints: &[TraitConstraint],
        engines: &Engines,
    ) -> Result<Vec<(TypeId, String)>, ErrorEmitted> {
        let type_id = engines.te().get_unaliased_type_id(type_id);

        let _decl_engine = engines.de();
        let unify_check = UnifyCheck::coercion(engines);
        let unify_check_equality = UnifyCheck::constraint_subset(engines);

        let mut impld_traits_type_ids: Vec<Vec<(TypeId, String)>> = vec![];
        let _ = module.walk_scope_chain(|lexical_scope| {
            let impls = lexical_scope
                .items
                .implemented_traits
                .get_impls(engines, type_id, true);
            impld_traits_type_ids.push(
                impls
                    .iter()
                    .filter_map(|e| {
                        let key = &e.key;
                        let mut res = None;
                        for constraint in constraints {
                            if key.name.suffix.name == constraint.trait_name.suffix
                                && key
                                    .name
                                    .suffix
                                    .args
                                    .iter()
                                    .zip(constraint.type_arguments.iter())
                                    .all(|(a1, a2)| {
                                        unify_check_equality.check(a1.type_id, a2.type_id)
                                    })
                                && unify_check.check(type_id, key.type_id)
                            {
                                let name_type_args = if !key.name.suffix.args.is_empty() {
                                    format!("<{}>", engines.help_out(key.name.suffix.args.clone()))
                                } else {
                                    "".to_string()
                                };
                                let name =
                                    format!("{}{}", key.name.suffix.name.as_str(), name_type_args);
                                res = Some((key.type_id, name));
                                break;
                            }
                        }
                        res
                    })
                    .collect(),
            );

            Ok(None::<()>)
        });
        Ok(impld_traits_type_ids.concat())
    }

    fn get_impls_mut(&mut self, engines: &Engines, type_id: TypeId) -> &mut Vec<TraitEntry> {
        let type_root_filter = Self::get_type_root_filter(engines, type_id);
        if !self.trait_impls.contains_key(&type_root_filter) {
            self.trait_impls
                .insert(type_root_filter.clone(), Vec::new());
        }

        self.trait_impls.get_mut(&type_root_filter).unwrap()
    }

    fn get_impls(
        &self,
        engines: &Engines,
        type_id: TypeId,
        extend_with_placeholder: bool,
    ) -> Vec<TraitEntry> {
        let type_root_filter = Self::get_type_root_filter(engines, type_id);
        let mut vec = self
            .trait_impls
            .get(&type_root_filter)
            .cloned()
            .unwrap_or_default();
        if extend_with_placeholder && type_root_filter != TypeRootFilter::Placeholder {
            vec.extend(
                self.trait_impls
                    .get(&TypeRootFilter::Placeholder)
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        vec
    }

    // Return a string representing only the base type.
    // This is used by the trait map to filter the entries into a HashMap with the return type string as key.
    fn get_type_root_filter(engines: &Engines, type_id: TypeId) -> TypeRootFilter {
        use TypeInfo::*;
        match &*engines.te().get(type_id) {
            Unknown => TypeRootFilter::Unknown,
            Never => TypeRootFilter::Never,
            UnknownGeneric { .. } | Placeholder(_) => TypeRootFilter::Placeholder,
            TypeParam(n) => TypeRootFilter::TypeParam(*n),
            StringSlice => TypeRootFilter::StringSlice,
            StringArray(x) => TypeRootFilter::StringArray(x.val()),
            UnsignedInteger(x) => match x {
                IntegerBits::Eight => TypeRootFilter::U8,
                IntegerBits::Sixteen => TypeRootFilter::U16,
                IntegerBits::ThirtyTwo => TypeRootFilter::U32,
                IntegerBits::SixtyFour => TypeRootFilter::U64,
                IntegerBits::V256 => TypeRootFilter::U256,
            },
            Boolean => TypeRootFilter::Bool,
            Custom {
                qualified_call_path: call_path,
                ..
            } => TypeRootFilter::Custom(call_path.call_path.suffix.to_string()),
            B256 => TypeRootFilter::B256,
            Numeric => TypeRootFilter::U64, // u64 is the default
            Contract => TypeRootFilter::Contract,
            ErrorRecovery(_) => TypeRootFilter::ErrorRecovery,
            Tuple(fields) => TypeRootFilter::Tuple(fields.len()),
            UntypedEnum(decl_id) => TypeRootFilter::Enum(*decl_id),
            UntypedStruct(decl_id) => TypeRootFilter::Struct(*decl_id),
            Enum(decl_id) => {
                // TODO Remove unwrap once #6475 is fixed
                TypeRootFilter::Enum(engines.de().get_parsed_decl_id(decl_id).unwrap())
            }
            Struct(decl_id) => {
                // TODO Remove unwrap once #6475 is fixed
                TypeRootFilter::Struct(engines.de().get_parsed_decl_id(decl_id).unwrap())
            }
            ContractCaller { abi_name, .. } => TypeRootFilter::ContractCaller(abi_name.to_string()),
            Array(_, length) => TypeRootFilter::Array(length.val()),
            RawUntypedPtr => TypeRootFilter::RawUntypedPtr,
            RawUntypedSlice => TypeRootFilter::RawUntypedSlice,
            Ptr(_) => TypeRootFilter::Ptr,
            Slice(_) => TypeRootFilter::Slice,
            Alias { ty, .. } => Self::get_type_root_filter(engines, ty.type_id),
            TraitType { name, .. } => TypeRootFilter::TraitType(name.to_string()),
            Ref {
                referenced_type, ..
            } => Self::get_type_root_filter(engines, referenced_type.type_id),
        }
    }
}
