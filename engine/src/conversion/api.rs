// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::vendored_bindgen::callbacks::{
    Explicitness, MethodKind as CppMethodKind, SpecialMemberKind, Virtualness,
};

use syn::{
    punctuated::Punctuated,
    token::{Comma, Unsafe},
};

use crate::types::{make_ident, Namespace, QualifiedName};
use crate::{
    minisyn::{
        Attribute, FnArg, Ident, ItemConst, ItemEnum, ItemStruct, ItemType, ReturnType, Type,
        Visibility,
    },
    parse_callbacks::CppOriginalName,
};
use autocxx_parser::{ExternCppType, IncludeCppConfig, RustFun, RustPath};
use indexmap::map::IndexMap as HashMap;
use itertools::Itertools;
use quote::ToTokens;

pub(crate) use crate::vendored_bindgen::callbacks::Visibility as CppVisibility;

use super::{
    analysis::fun::{
        function_wrapper::{CppFunction, CppFunctionBody, CppFunctionKind, TypeConversionPolicy},
        ReceiverMutability,
    },
    convert_error::{ConvertErrorWithContext, ErrorContext},
    parse::CppRefQualifier,
    ConvertErrorFromCpp, CppEffectiveName,
};

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum TypeKind {
    Pod,    // trivial. Can be moved and copied in Rust.
    NonPod, // has destructor or non-trivial move constructors. Can only hold by UniquePtr
    Opaque, // bindgen opaque data. We can't generate any constructors as we don't
    // know what fields it has.
    // NB you might not find references to this in the codebase - that's
    // because `implicit_constructor.rs` acts on all the other types of TypeKind
    Abstract, // has pure virtual members - can't even generate UniquePtr.
              // It's possible that the type itself isn't pure virtual, but it inherits from
              // some other type which is pure virtual. Alternatively, maybe we just don't
              // know if the base class is pure virtual because it wasn't on the allowlist,
              // in which case we'll err on the side of caution.
}

/// One of the C++ helper functions autocxx generates beside the opaque holder
/// it lowers a `std::shared_ptr<const T>` to.
///
/// cxx sees that holder as an ordinary opaque extern type, so nothing about it
/// being a smart pointer reaches Rust by itself: these three shims are the
/// whole of what a caller can do with one. Their names are derived from the
/// holder's, here, because the Rust and C++ halves of the codegen each write
/// one end of the same declaration and have to agree on it. See
/// google/autocxx#799.
#[derive(Copy, Clone)]
pub(crate) enum SharedPtrShim {
    /// `std::shared_ptr::get`. The payload is `const` in C++, so this reaches
    /// Rust as a `*const T` - or a `CppRef<T>` under
    /// `ReferencesWrappedAllFunctionsSafe` - and never as a `&mut`.
    Get,
    /// Copy-construction of the holder, which is what makes a second owner of
    /// the same payload and raises the reference count.
    Clone,
    /// `std::shared_ptr::use_count`, returned as an `int64_t` rather than the
    /// `long` C++ declares: `long` is 32 bits on Windows, and cxx would
    /// typecheck the shim against the wrong signature there.
    UseCount,
}

impl SharedPtrShim {
    pub(crate) const ALL: [Self; 3] = [Self::Get, Self::Clone, Self::UseCount];

    /// What the method is called on the Rust side, and the tail of what the
    /// C++ function is called.
    pub(crate) fn rust_name(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Clone => "clone",
            Self::UseCount => "use_count",
        }
    }

    /// The C++ function's name, and the name the `cxx::bridge` declares it by.
    ///
    /// Built rather than allocated, so unlike every other bridge name it is not
    /// reserved against the user's own: `BridgeNameTracker` and
    /// `fixed_bridge_names` run during analysis, and the holder these belong to
    /// is manufactured after that. A header declaring a function called
    /// `<holder>_autocxx_get` would collide, where `<holder>` is the mangled
    /// spelling of a `std::shared_ptr<const T>` instantiation - so the name to
    /// collide with is one nobody writes by accident, and a collision is a Rust
    /// compile error in generated code rather than anything silent.
    pub(crate) fn cpp_name(self, holder: &QualifiedName) -> String {
        format!("{}_autocxx_{}", holder.get_final_item(), self.rust_name())
    }
}

/// Details about a C++ struct.
#[derive(Debug)]
pub(crate) struct StructDetails {
    pub(crate) item: ItemStruct,
    pub(crate) has_rvalue_reference_fields: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CastMutability {
    ConstToConst,
    MutToConst,
    MutToMut,
}

/// Indicates that this function (which is synthetic) should
/// be a trait implementation rather than a method or free function.
#[derive(Clone, Debug)]
pub(crate) enum TraitSynthesis {
    Cast {
        to_type: QualifiedName,
        mutable: CastMutability,
    },
    AllocUninitialized(QualifiedName),
    FreeUninitialized(QualifiedName),
}

/// Details of a subclass constructor.
///
/// One synthesized [`Api::Function`] carries two separate things: the
/// bridge-visible wrapper Rust calls, and - in `cpp_impl` - the peer class's
/// own C++ constructor. The two are consumed independently: the C++ codegen's
/// `add_needs` collects `cpp_impl` into the constructors it writes into the
/// peer class, and generates the wrapper separately, while the Rust codegen's
/// `find_trivially_constructed_subclasses` reads `is_trivial` to decide which
/// subclasses get a `CppPeerConstructor` impl. Neither uses what the other
/// does.
///
/// The tidier shape is a separate [`Api`] variant for the peer's constructor.
/// The cost of one is not in the variant itself. Some of it the compiler
/// asks for, at every exhaustive match over [`Api`] - `Api::name_info`,
/// `error_reporter::convert_apis`, `check_names`,
/// `RsCodeGenerator::generate_rs_for_api`. The rest it doesn't: both
/// `HasDependencies::deps` impls and `needs_cpp_codegen` fall through to a
/// default, so a variant nobody remembered to add there is silently one with
/// no dependencies and no C++ to generate. That is worth paying once, as part
/// of a broader cleanup of how synthesized APIs are represented, rather than
/// for this one struct.
///
/// Decision: this stays as it is until that cleanup happens.
#[derive(Clone, Debug)]
pub(crate) struct SubclassConstructorDetails {
    pub(crate) subclass: SubclassName,
    pub(crate) is_trivial: bool,
    /// Implementation of the constructor _itself_ as distinct
    /// from any wrapper function we create to call it.
    pub(crate) cpp_impl: CppFunction,
}

/// Contributions to traits representing C++ superclasses that
/// we may implement as Rust subclasses.
#[derive(Clone, Debug)]
pub(crate) struct SuperclassMethod {
    pub(crate) name: Ident,
    pub(crate) receiver: QualifiedName,
    pub(crate) params: Punctuated<FnArg, Comma>,
    /// How each of `params` was converted for the Rust-calls-C++ direction,
    /// receiver included, so that codegen can undo the conversion for the
    /// direction a `subclass!` override is really called in. See
    /// [`TypeConversionPolicy::inverse_rust_conversion`].
    pub(crate) param_conversions: Vec<TypeConversionPolicy>,
    pub(crate) ret_type: ReturnType,
    /// How the return value was converted for the Rust-calls-C++ direction, so
    /// that codegen can undo that conversion too - a `subclass!` override
    /// produces this value rather than receiving it. See
    /// [`TypeConversionPolicy::inverse_rust_return_conversion`].
    pub(crate) ret_conversion: Option<TypeConversionPolicy>,
    pub(crate) receiver_mutability: ReceiverMutability,
    pub(crate) requires_unsafe: UnsafetyNeeded,
    /// Whether the peer class offers a `foo_super` helper by which a Rust
    /// subclass can call the superclass's own implementation of this method.
    /// It doesn't if the method is pure virtual (there is no implementation)
    /// or `private` (C++ lets a derived class override such a method but not
    /// call it), and then the `_methods` trait item has no default body and
    /// every Rust subclass has to implement it.
    pub(crate) has_super_helper: bool,
    /// How to call the superclass's own binding for this method, so that the
    /// superclass can implement its own `_methods` trait alongside its
    /// subclasses. Whether such a binding exists at all is not settled until
    /// codegen; see <https://github.com/google/autocxx/issues/609>.
    pub(crate) superclass_binding: SuperclassBinding,
}

/// Why a typedef ended up as an [`Api::OpaqueTypedef`]: which of the things it
/// was defined in terms of autocxx could not generate, and that thing's own
/// reason. Kept so that the failure a user reads about is the one that
/// actually happened, rather than the fact that a stand-in type was used.
#[derive(Clone, Debug)]
pub(crate) struct OpaqueTypedefReason {
    pub(crate) culprit: QualifiedName,
    pub(crate) reason: Box<ConvertErrorFromCpp>,
}

/// How the superclass's own binding for a virtual method differs from the
/// shape its `_methods` trait uses. The two are generated from the same
/// C++ method, so the parameters always line up - a `UniquePtr<T>` is
/// accepted wherever the superclass's binding asks for an
/// `impl ValueParam<T>` or an `impl ToCppString` - but the return value
/// need not.
#[derive(Clone, Debug)]
pub(crate) struct SuperclassBinding {
    /// The superclass's binding hands back an `impl New`, where the trait
    /// deals in `UniquePtr`, so forwarding to it has to emplace the result.
    pub(crate) returns_new: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct TraitImplSignature {
    pub(crate) ty: Type,
    pub(crate) trait_signature: Type,
    /// The trait is 'unsafe' itself
    pub(crate) unsafety: Option<Unsafe>,
}

impl Eq for TraitImplSignature {}

impl PartialEq for TraitImplSignature {
    fn eq(&self, other: &Self) -> bool {
        totokens_equal(&self.unsafety, &other.unsafety)
            && totokens_equal(&self.ty, &other.ty)
            && totokens_equal(&self.trait_signature, &other.trait_signature)
    }
}

fn totokens_to_string<T: ToTokens>(a: &T) -> String {
    a.to_token_stream().to_string()
}

fn totokens_equal<T: ToTokens>(a: &T, b: &T) -> bool {
    totokens_to_string(a) == totokens_to_string(b)
}

fn hash_totokens<T: ToTokens, H: std::hash::Hasher>(a: &T, state: &mut H) {
    use std::hash::Hash;
    totokens_to_string(a).hash(state)
}

impl std::hash::Hash for TraitImplSignature {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        hash_totokens(&self.ty, state);
        hash_totokens(&self.trait_signature, state);
        hash_totokens(&self.unsafety, state);
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Provenance {
    Bindgen,
    SynthesizedOther,
    SynthesizedSubclassConstructor(Box<SubclassConstructorDetails>),
}

/// A C++ function for which we need to generate bindings, but haven't
/// yet analyzed in depth. This is little more than a `ForeignItemFn`
/// broken down into its constituent parts, plus some metadata from the
/// surrounding bindgen parsing context.
///
/// Some parts of the code synthesize additional functions and then
/// pass them through the same pipeline _as if_ they were discovered
/// during normal bindgen parsing. If that happens, they'll create one
/// of these structures, and typically fill in some of the
/// `synthesized_*` members which are not filled in from bindgen.
#[derive(Clone, Debug)]
pub(crate) struct FuncToConvert {
    pub(crate) provenance: Provenance,
    pub(crate) ident: Ident,
    pub(crate) doc_attrs: Vec<Attribute>,
    pub(crate) inputs: Punctuated<FnArg, Comma>,
    pub(crate) variadic: bool,
    pub(crate) output: ReturnType,
    pub(crate) vis: Visibility,
    pub(crate) virtualness: Option<Virtualness>,
    pub(crate) cpp_vis: CppVisibility,
    pub(crate) special_member: Option<SpecialMemberKind>,
    /// What C++ says this is: a constructor, a destructor, a static method or
    /// an ordinary one. `None` for a free function, which has no method kind,
    /// and for a function autocxx synthesized, which bindgen never saw.
    pub(crate) method_kind: Option<CppMethodKind>,
    pub(crate) original_name: Option<CppOriginalName>,
    /// Used for static functions only. For all other functons,
    /// this is figured out from the receiver type in the inputs.
    pub(crate) self_ty: Option<QualifiedName>,
    /// If we wish to use a different 'this' type than the original
    /// method receiver, e.g. because we're making a subclass
    /// constructor, fill it in here.
    pub(crate) synthesized_this_type: Option<QualifiedName>,
    /// If this function should actually belong to a trait.
    pub(crate) add_to_trait: Option<TraitSynthesis>,
    /// If Some, this function didn't really exist in the original
    /// C++ and instead we're synthesizing it.
    pub(crate) synthetic_cpp: Option<(CppFunctionBody, CppFunctionKind)>,
    /// =delete or =default
    pub(crate) is_deleted: Option<Explicitness>,
    /// Whether this is a `void foo() &` or `void foo() &&` method. Recovered
    /// from the mangled name, because bindgen doesn't tell us; see
    /// [`crate::conversion::parse::CppRefQualifier`]. Always
    /// [`CppRefQualifier::None`] for functions we synthesize ourselves.
    pub(crate) ref_qualifier: CppRefQualifier,
}

/// Layers of analysis which may be applied to decorate each API.
/// See description of the purpose of this trait within `Api`.
pub(crate) trait AnalysisPhase: std::fmt::Debug {
    type TypedefAnalysis: std::fmt::Debug;
    type StructAnalysis: std::fmt::Debug;
    type FunAnalysis: std::fmt::Debug;
    type SubclassAnalysis: std::fmt::Debug;
}

/// No analysis has been applied to this API.
#[derive(std::fmt::Debug)]
pub(crate) struct NullPhase;

impl AnalysisPhase for NullPhase {
    type TypedefAnalysis = ();
    type StructAnalysis = ();
    type FunAnalysis = ();
    type SubclassAnalysis = ();
}

#[derive(Clone, Debug)]
pub(crate) enum TypedefKind {
    Use(Box<Type>),
    Type(Box<ItemType>),
}

/// Name information for an API. This includes the name by
/// which we know it in Rust, and its C++ name, which may differ.
#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct ApiName {
    pub(crate) name: QualifiedName,
    cpp_name: Option<CppOriginalName>,
}

impl ApiName {
    pub(crate) fn new(ns: &Namespace, id: Ident) -> Self {
        Self::new_from_qualified_name(QualifiedName::new(ns, id))
    }

    pub(crate) fn new_with_cpp_name(
        ns: &Namespace,
        id: Ident,
        cpp_name: Option<CppOriginalName>,
    ) -> Self {
        Self::new_from_qualified_name_and_cpp_name(QualifiedName::new(ns, id), cpp_name)
    }

    /// For callers which have already built the [`QualifiedName`] - typically
    /// because they needed it to look the C++ name up in the first place.
    pub(crate) fn new_from_qualified_name_and_cpp_name(
        name: QualifiedName,
        cpp_name: Option<CppOriginalName>,
    ) -> Self {
        Self { name, cpp_name }
    }

    pub(crate) fn new_from_qualified_name(name: QualifiedName) -> Self {
        Self::new_from_qualified_name_and_cpp_name(name, None)
    }

    pub(crate) fn new_in_root_namespace(id: Ident) -> Self {
        Self::new(&Namespace::new(), id)
    }

    /// Return the C++ name to use here, which will use the Rust
    /// name unless it's been overridden by a C++ name.
    pub(crate) fn cpp_name(&self) -> CppEffectiveName {
        CppEffectiveName::from_api_details(&self.cpp_name, &self.name)
    }

    pub(crate) fn cpp_name_if_present(&self) -> Option<&CppOriginalName> {
        self.cpp_name.as_ref()
    }

    /// Every C++ spelling by which one of the user's directives might name
    /// this API.
    ///
    /// Usually there is just the one. A *nested* type has two, because bindgen
    /// flattens the nesting: a `struct Inner` inside a `struct Outer` in
    /// namespace `ns` reaches us as `ns::Outer_Inner`, which is what we call it
    /// everywhere in Rust, while C++ itself calls it `ns::Outer::Inner`. Nobody
    /// writing `generate!`, `pod!` or `extern_cpp_type!` has any reason to know
    /// about the flattened spelling, so we accept either.
    /// See google/autocxx#1422.
    /// The spelling C++ itself uses, when that differs from the flattened one
    /// [`QualifiedName::to_cpp_name`] produces - that is, for a nested type.
    pub(crate) fn nested_cpp_spelling(&self) -> Option<String> {
        self.cpp_name
            .as_ref()
            .filter(|cpp_name| cpp_name.is_nested())
            .map(|cpp_name| {
                self.name
                    .get_namespace()
                    .iter()
                    .chain(std::iter::once(cpp_name.for_original_name_map()))
                    .join("::")
            })
    }

    pub(crate) fn cpp_spellings(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(self.name.to_cpp_name()).chain(self.nested_cpp_spelling())
    }
}

/// The spelling C++ uses for each nested type, so that a directive naming one
/// that way can be matched against the flattened name we know it by.
///
/// Only nested types appear here: everything else answers to
/// [`QualifiedName::to_cpp_name`] alone, which is what
/// [`Self::is_on_allowlist`] falls back on.
pub(crate) struct NestedCppNames<'a> {
    config: &'a IncludeCppConfig,
    spellings: HashMap<QualifiedName, String>,
}

impl<'a> NestedCppNames<'a> {
    pub(crate) fn new<'n>(
        config: &'a IncludeCppConfig,
        names: impl Iterator<Item = &'n ApiName>,
    ) -> Self {
        Self {
            config,
            spellings: names
                .filter_map(|name| {
                    name.nested_cpp_spelling()
                        .map(|spelling| (name.name.clone(), spelling))
                })
                .collect(),
        }
    }

    /// Every C++ spelling this type answers to: the flattened name always,
    /// and the one C++ uses as well if the two differ.
    pub(crate) fn spellings<'s>(
        &'s self,
        name: &QualifiedName,
    ) -> impl Iterator<Item = String> + 's {
        std::iter::once(name.to_cpp_name()).chain(self.spellings.get(name).cloned())
    }

    /// Whether one of the user's allowlist directives names this type, by
    /// either of the spellings it answers to. See google/autocxx#1422.
    pub(crate) fn is_on_allowlist(&self, name: &QualifiedName) -> bool {
        self.spellings(name)
            .any(|spelling| self.config.is_on_allowlist(&spelling))
    }
}

impl std::fmt::Debug for ApiName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(cpp_name) = &self.cpp_name {
            write!(f, " (cpp={cpp_name})")?;
        }
        Ok(())
    }
}

/// A name representing a subclass.
/// This is a simple newtype wrapper which exists such that
/// we can consistently generate the names of the various subsidiary
/// types which are required both in C++ and Rust codegen.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub(crate) struct SubclassName(pub(crate) ApiName);

/// What we append to a method name to name the means of calling the
/// superclass's own implementation of it.
pub(crate) const SUPER_FN_SUFFIX: &str = "_super";

impl SubclassName {
    pub(crate) fn new(id: Ident) -> Self {
        Self(ApiName::new_in_root_namespace(id))
    }
    pub(crate) fn from_holder_name(id: &Ident) -> Self {
        Self::new(make_ident(id.to_string().strip_suffix("Holder").unwrap()))
    }
    pub(crate) fn id(&self) -> Ident {
        self.0.name.get_final_ident()
    }
    /// Generate the name for the 'Holder' type
    pub(crate) fn holder(&self) -> Ident {
        self.with_suffix("Holder")
    }
    /// Generate the name for the 'Cpp' type
    pub(crate) fn cpp(&self) -> QualifiedName {
        let id = self.with_suffix("Cpp");
        QualifiedName::new(self.0.name.get_namespace(), id)
    }
    pub(crate) fn cpp_remove_ownership(&self) -> Ident {
        self.with_suffix("Cpp_remove_ownership")
    }
    pub(crate) fn remove_ownership(&self) -> Ident {
        self.with_suffix("_remove_ownership")
    }
    fn with_suffix(&self, suffix: &str) -> Ident {
        make_ident(format!("{}{}", self.0.name.get_final_item(), suffix))
    }
    pub(crate) fn get_trait_api_name(sup: &QualifiedName, method_name: &str) -> QualifiedName {
        QualifiedName::new(
            sup.get_namespace(),
            make_ident(format!(
                "{}_{}_trait_item",
                sup.get_final_item(),
                method_name
            )),
        )
    }
    /// The Rust name of the method on a subclass's peer class which calls the
    /// superclass implementation of `id` - `foo_super` for `foo`. Subclass
    /// authors write this, as `self.peer().foo_super(..)`, so it stays plain.
    /// Nothing else on the peer class can collide with it: the peer's C++
    /// overrides of the superclass's methods are called from C++ only.
    pub(crate) fn get_super_fn_name(superclass_namespace: &Namespace, id: &str) -> QualifiedName {
        let id = make_ident(format!("{id}{SUPER_FN_SUFFIX}"));
        QualifiedName::new(superclass_namespace, id)
    }
    /// The C++ name of that same method, which has to differ: the peer class
    /// declares each of the superclass's virtual methods under its real C++
    /// name in order to override it, and one of those may well be called
    /// `foo_super` already. So this name carries an `autocxx` marker, and cxx
    /// bridges the two names with a `#[cxx_name]`.
    ///
    /// One marker is not by itself enough - a superclass is free to have a
    /// method called `foo_autocxx_super` too, and then the peer class would
    /// declare that name twice. So keep adding markers until `is_taken` says
    /// the name is free, which is what the Rust `_supers` trait does with the
    /// plain `foo_super` spelling. The caller owns that judgement because only
    /// it knows which names the peer class has already spoken for.
    pub(crate) fn get_cpp_super_fn_name(
        superclass_namespace: &Namespace,
        id: &str,
        mut is_taken: impl FnMut(&str) -> bool,
    ) -> QualifiedName {
        let mut marks = String::new();
        let mut candidate;
        loop {
            marks.push_str("_autocxx");
            candidate = format!("{id}{marks}{SUPER_FN_SUFFIX}");
            if !is_taken(&candidate) {
                break;
            }
        }
        QualifiedName::new(superclass_namespace, make_ident(candidate))
    }
    pub(crate) fn get_methods_trait_name(superclass_name: &QualifiedName) -> QualifiedName {
        Self::with_qualified_name_suffix(superclass_name, "methods")
    }
    pub(crate) fn get_supers_trait_name(superclass_name: &QualifiedName) -> QualifiedName {
        Self::with_qualified_name_suffix(superclass_name, "supers")
    }

    fn with_qualified_name_suffix(name: &QualifiedName, suffix: &str) -> QualifiedName {
        let id = make_ident(format!("{}_{}", name.get_final_item(), suffix));
        QualifiedName::new(name.get_namespace(), id)
    }
}

#[derive(std::fmt::Debug)]
/// Different types of API we might encounter.
///
/// This type is parameterized over an `ApiAnalysis`. This is any additional
/// information which we wish to apply to our knowledge of our APIs later
/// during analysis phases.
///
/// This is not as high-level as the equivalent types in `cxx` or `bindgen`,
/// because sometimes we pass on the `bindgen` output directly in the
/// Rust codegen output.
///
/// Any `syn` types represented in this `Api` type, or any of the types from
/// which it is composed, should be wrapped in `crate::minisyn` equivalents
/// to avoid excessively verbose `Debug` logging.
pub(crate) enum Api<T: AnalysisPhase> {
    /// A forward declaration, which we mustn't store in a UniquePtr.
    ForwardDeclaration {
        name: ApiName,
        /// If we found a problem parsing this forward declaration, we'll
        /// ephemerally store the error here, as opposed to immediately
        /// converting it to an `IgnoredItem`. That's because the
        /// 'replace_hopeless_typedef_targets' analysis phase needs to spot
        /// cases where there was an error which was _also_ a forward declaration.
        /// That phase will then discard such Api::ForwardDeclarations
        /// and replace them with normal Api::IgnoredItems.
        err: Option<ConvertErrorWithContext>,
    },
    /// We found a typedef to something that we didn't fully understand.
    /// We'll treat it as an opaque unsized type.
    OpaqueTypedef {
        name: ApiName,
        /// Further store whether this was a typedef to a forward declaration.
        /// If so we can't allow it to live in a UniquePtr, just like a regular
        /// Api::ForwardDeclaration.
        forward_declaration: bool,
        /// What was wrong with the thing this typedef named, where we know.
        /// Anything which goes on to use the typedef has to be refused, and
        /// this is what it gets told - see
        /// [`ConvertErrorFromCpp::TypeContainingUngeneratableTypedef`].
        reason: Option<OpaqueTypedefReason>,
    },
    /// A synthetic type we've manufactured in order to
    /// concretize some templated C++ type.
    ConcreteType {
        name: ApiName,
        rs_definition: Option<Box<Type>>,
        cpp_definition: String,
        /// Where this concrete type is the opaque holder we lower a
        /// `std::shared_ptr<const T>` to, the `T` as the `cxx::bridge` spells
        /// it. `None` for every other concrete type. See google/autocxx#799.
        shared_ptr_payload: Option<Box<Type>>,
    },
    /// A simple note that we want to make a constructor for
    /// a `std::string` on the heap.
    StringConstructor { name: ApiName },
    /// A function. May include some analysis.
    Function {
        name: ApiName,
        fun: Box<FuncToConvert>,
        analysis: T::FunAnalysis,
    },
    /// A constant.
    Const {
        name: ApiName,
        const_item: ItemConst,
    },
    /// A variable with static storage duration: either a C++ variable at
    /// namespace scope, or a static data member of a class. `bindgen`
    /// declares these as `extern "C"` statics inside the mod we emit
    /// verbatim, so all we have to do is re-export them - see
    /// google/autocxx#93.
    Static {
        name: ApiName,
        /// The C++ type of the variable, if it is a type which `autocxx`
        /// itself generates (as opposed to a plain Rust type such as
        /// `std::os::raw::c_int`, which `bindgen` emits directly). We record
        /// it so that the garbage collector keeps that type alive, and so
        /// that we can insist it is POD before re-exporting the variable.
        cpp_ty: Option<QualifiedName>,
    },
    /// A typedef found in the bindgen output which we wish
    /// to pass on in our output
    Typedef {
        name: ApiName,
        item: TypedefKind,
        old_tyname: Option<QualifiedName>,
        analysis: T::TypedefAnalysis,
    },
    /// An enum encountered in the
    /// `bindgen` output.
    Enum { name: ApiName, item: ItemEnum },
    /// A struct encountered in the
    /// `bindgen` output.
    Struct {
        name: ApiName,
        details: Box<StructDetails>,
        analysis: T::StructAnalysis,
    },
    /// A C type the generated C++ has to typedef for itself, because cxx has
    /// no spelling of its own for it: the variable-length integers (`int`,
    /// `unsigned long`), `void`, and `char16_t`. See `KnownTypes::as_ctype`
    /// for which types those are; `typename` is the canonical name to declare
    /// it under, which need not be the alias it reached us as.
    CType {
        name: ApiName,
        typename: QualifiedName,
    },
    /// Some item which couldn't be processed by autocxx for some reason.
    /// We will have emitted a warning message about this, but we want
    /// to mark that it's ignored so that we don't attempt to process
    /// dependent items.
    IgnoredItem {
        name: ApiName,
        err: ConvertErrorFromCpp,
        ctx: Option<ErrorContext>,
    },
    /// A Rust type which is not a C++ type.
    RustType { name: ApiName, path: RustPath },
    /// A function for the 'extern Rust' block which is not a C++ type.
    RustFn {
        name: ApiName,
        details: RustFun,
        deps: Vec<QualifiedName>,
    },
    /// Some function for the extern "Rust" block.
    RustSubclassFn {
        name: ApiName,
        subclass: SubclassName,
        details: Box<RustSubclassFnDetails>,
    },
    /// A Rust subclass of a C++ class.
    Subclass {
        name: SubclassName,
        superclass: QualifiedName,
        analysis: T::SubclassAnalysis,
    },
    /// Contributions to the traits representing superclass methods that we might
    /// subclass in Rust.
    SubclassTraitItem {
        name: ApiName,
        details: SuperclassMethod,
    },
    /// A type which we shouldn't ourselves generate, but can use in functions
    /// and so-forth by referring to some definition elsewhere.
    ExternCppType {
        name: ApiName,
        details: ExternCppType,
        pod: bool,
    },
}

#[derive(Debug)]
pub(crate) struct RustSubclassFnDetails {
    pub(crate) params: Punctuated<FnArg, Comma>,
    pub(crate) ret: ReturnType,
    pub(crate) cpp_impl: CppFunction,
    pub(crate) method_name: Ident,
    pub(crate) superclass: QualifiedName,
    pub(crate) receiver_mutability: ReceiverMutability,
    pub(crate) dependencies: Vec<QualifiedName>,
    pub(crate) requires_unsafe: UnsafetyNeeded,
    /// The C++ name of the peer class's `_super` helper for this method, or
    /// `None` if it doesn't get one - see
    /// [`SuperclassMethod::has_super_helper`].
    ///
    /// Settled during analysis rather than recomputed at codegen, because the
    /// same name also has to appear in the cxx bridge as a `#[cxx_name]`: two
    /// derivations of it could disagree, and then Rust would call a C++
    /// function which doesn't exist.
    pub(crate) cpp_super_fn_name: Option<Ident>,
}

#[derive(Clone, Debug)]
pub(crate) enum UnsafetyNeeded {
    None,
    JustBridge,
    Always,
}

impl<T: AnalysisPhase> Api<T> {
    pub(crate) fn name_info(&self) -> &ApiName {
        match self {
            Api::ForwardDeclaration { name, .. } => name,
            Api::OpaqueTypedef { name, .. } => name,
            Api::ConcreteType { name, .. } => name,
            Api::StringConstructor { name } => name,
            Api::Function { name, .. } => name,
            Api::Const { name, .. } => name,
            Api::Static { name, .. } => name,
            Api::Typedef { name, .. } => name,
            Api::Enum { name, .. } => name,
            Api::Struct { name, .. } => name,
            Api::CType { name, .. } => name,
            Api::IgnoredItem { name, .. } => name,
            Api::RustType { name, .. } => name,
            Api::RustFn { name, .. } => name,
            Api::RustSubclassFn { name, .. } => name,
            Api::Subclass { name, .. } => &name.0,
            Api::SubclassTraitItem { name, .. } => name,
            Api::ExternCppType { name, .. } => name,
        }
    }

    /// The name of this API as used in Rust code.
    /// For types, it's important that this never changes, since
    /// functions or other types may refer to this.
    /// Yet for functions, this may not actually be the name
    /// used in the [cxx::bridge] mod -  see
    /// [Api<FnAnalysis>::cxxbridge_name]
    pub(crate) fn name(&self) -> &QualifiedName {
        &self.name_info().name
    }

    /// The name recorded for use in C++, if and only if
    /// it differs from Rust.
    pub(crate) fn cpp_name(&self) -> &Option<CppOriginalName> {
        &self.name_info().cpp_name
    }

    /// The name for use in C++, whether or not it differs
    /// from Rust.
    pub(crate) fn effective_cpp_name(&self) -> CppEffectiveName {
        self.name_info().cpp_name()
    }

    /// If this API turns out to have the same QualifiedName as another,
    /// whether it's OK to just discard it?
    ///
    /// A variable can clash with a type of the same name - C++ allows
    /// `struct stat {...}; extern struct stat stat;` and `bindgen` calls both
    /// of them `stat` - and in that case we would much rather keep the type,
    /// since other APIs may depend on it.
    ///
    /// [`ApiVec::push`] only asks this of the API being added, so the answer
    /// keeps the type only if the type was added first. It always is:
    /// `ParseBindgen::parse_mod_items` pushes each struct as it walks the mod,
    /// and hands over the mod's variables afterwards, in
    /// `ParseForeignMod::finished`.
    ///
    /// The tests for this live in `apivec`, not the integration suite, because
    /// such a header can't get as far as Rust: our generated C++ names a type
    /// without the `struct` tag which would rescue it, so `new_appropriately
    /// <stat>` and cxx's `is_complete<::stat>` both resolve to the variable
    /// and fail to compile. That is true whether or not we make an [`Api`] for
    /// the variable at all, so it's a separate limitation of our C++ codegen.
    ///
    /// [`ApiVec::push`]: crate::conversion::apivec::ApiVec::push
    pub(crate) fn discard_duplicates(&self) -> bool {
        matches!(self, Api::IgnoredItem { .. } | Api::Static { .. })
    }

    pub(crate) fn valid_types(&self) -> Box<dyn Iterator<Item = QualifiedName>> {
        match self {
            Api::Subclass { name, .. } => Box::new(
                vec![
                    self.name().clone(),
                    QualifiedName::new(&Namespace::new(), name.holder()),
                    name.cpp(),
                ]
                .into_iter(),
            ),
            _ => Box::new(std::iter::once(self.name().clone())),
        }
    }
}

pub(crate) type UnanalyzedApi = Api<NullPhase>;

impl<T: AnalysisPhase> Api<T> {
    pub(crate) fn typedef_unchanged(
        name: ApiName,
        item: TypedefKind,
        old_tyname: Option<QualifiedName>,
        analysis: T::TypedefAnalysis,
    ) -> Result<Box<dyn Iterator<Item = Api<T>>>, ConvertErrorWithContext>
    where
        T: 'static,
    {
        Ok(Box::new(std::iter::once(Api::Typedef {
            name,
            item,
            old_tyname,
            analysis,
        })))
    }

    pub(crate) fn struct_unchanged(
        name: ApiName,
        details: Box<StructDetails>,
        analysis: T::StructAnalysis,
    ) -> Result<Box<dyn Iterator<Item = Api<T>>>, ConvertErrorWithContext>
    where
        T: 'static,
    {
        Ok(Box::new(std::iter::once(Api::Struct {
            name,
            details,
            analysis,
        })))
    }

    pub(crate) fn fun_unchanged(
        name: ApiName,
        fun: Box<FuncToConvert>,
        analysis: T::FunAnalysis,
    ) -> Result<Box<dyn Iterator<Item = Api<T>>>, ConvertErrorWithContext>
    where
        T: 'static,
    {
        Ok(Box::new(std::iter::once(Api::Function {
            name,
            fun,
            analysis,
        })))
    }

    pub(crate) fn enum_unchanged(
        name: ApiName,
        item: ItemEnum,
    ) -> Result<Box<dyn Iterator<Item = Api<T>>>, ConvertErrorWithContext>
    where
        T: 'static,
    {
        Ok(Box::new(std::iter::once(Api::Enum { name, item })))
    }

    pub(crate) fn subclass_unchanged(
        name: SubclassName,
        superclass: QualifiedName,
        analysis: T::SubclassAnalysis,
    ) -> Result<Box<dyn Iterator<Item = Api<T>>>, ConvertErrorWithContext>
    where
        T: 'static,
    {
        Ok(Box::new(std::iter::once(Api::Subclass {
            name,
            superclass,
            analysis,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::SubclassName;
    use crate::types::Namespace;
    use std::collections::HashSet;

    /// The escape which keeps a peer class's `_super` helper clear of the
    /// superclass method names the peer also has to declare.
    #[test]
    fn cpp_super_fn_name_dodges_names_already_spoken_for() {
        let taken: HashSet<&str> = ["foo_autocxx_super", "foo_autocxx_autocxx_super"]
            .into_iter()
            .collect();
        let name = |id| {
            SubclassName::get_cpp_super_fn_name(&Namespace::new(), id, |c| taken.contains(c))
                .get_final_item()
                .to_string()
        };
        // Nothing in the way: one marker is enough.
        assert_eq!(name("bar"), "bar_autocxx_super");
        // Two names in the way, so it takes three markers to get clear.
        assert_eq!(name("foo"), "foo_autocxx_autocxx_autocxx_super");
    }
}
