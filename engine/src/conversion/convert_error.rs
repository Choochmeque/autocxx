// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::set::IndexSet as HashSet;

use crate::minisyn::Ident;
use itertools::Itertools;
use miette::{Diagnostic, SourceSpan};
use proc_macro2::Span;
use thiserror::Error;

use crate::{
    known_types, proc_macro_span_to_miette_span,
    types::{
        make_ident, validate_str_ok_for_rust, InvalidIdentError, Namespace, QualifiedName,
        STD_FUNCTION_ADVICE,
    },
};

/// Errors which can occur during conversion
#[derive(Debug, Clone, Error, Diagnostic)]
pub enum ConvertError {
    #[error("The initial run of 'bindgen' did not generate any content. This might be because none of the requested items for generation could be converted.")]
    NoContent,
    #[error(transparent)]
    Cpp(ConvertErrorFromCpp),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Rust(LocatedConvertErrorFromRust),
}

/// Errors that can occur during conversion which are detected from some C++
/// source code. Currently, we do not gain span information from bindgen
/// so these errors are presented without useful source code snippets.
/// We hope to change this in future.
#[derive(Debug, Clone, Error)]
pub enum ConvertErrorFromCpp {
    #[error("An item was requested using 'generate_pod' which was not safe to hold by value in Rust. {0}")]
    UnsafePodType(String),
    #[error("Bindgen generated some unexpected code in a foreign mod section. You may have specified something in a 'generate' directive which is not currently compatible with autocxx.")]
    UnexpectedForeignItem,
    #[error("Bindgen generated some unexpected code in an inner namespace mod. You may have specified something in a 'generate' directive which is not currently compatible with autocxx.")]
    UnexpectedItemInMod,
    #[error("autocxx was unable to produce a typdef pointing to the complex type {0}.")]
    ComplexTypedefTarget(String),
    #[error("Unexpected type for 'this' in the function {}.", .0.to_cpp_name())]
    UnexpectedThisType(QualifiedName),
    #[error("autocxx does not yet know how to support the built-in C++ type {} - please raise an issue on github", .0.to_cpp_name())]
    UnsupportedBuiltInType(QualifiedName),
    #[error("Type {} has templated arguments and so does the typedef to which it points", .0.to_cpp_name())]
    ConflictingTemplatedArgsWithTypedef(QualifiedName),
    #[error("Function {0} has a parameter or return type which is either on the blocklist or a forward declaration")]
    UnacceptableParam(String),
    #[error("Function {0} has a reference return value, but no reference parameters, so the lifetime of the output reference cannot be deduced.")]
    NoInputReference(String),
    #[error("Function {0} has a reference return value, but >1 input reference parameters, so the lifetime of the output reference cannot be deduced.")]
    MultipleInputReferences(String),
    #[error("Function {0} has a mutable reference return value, but no mutable reference parameters, so the lifetime of the output reference cannot be deduced.")]
    NoMutableInputReference(String),
    #[error("Function {0} has a mutable reference return value, but >1 input mutable reference parameters, so the lifetime of the output reference cannot be deduced.")]
    MultipleMutableInputReferences(String),
    #[error("Encountered type not yet supported by autocxx: {0}")]
    UnsupportedType(String),
    #[error("Encountered type not yet known by autocxx: {0}")]
    UnknownType(String),
    #[error("This signature mentions C++'s `long double`, which autocxx does not pass between the languages, for a reason which differs by target. Where it is 8 bytes wide (MSVC, Apple Arm) it is a `double` under another name, and cxx checks the C++ function's exact type against the one it was told about, so declaring a `f64` is rejected by the C++ compiler. Where it is 16 bytes wide it is an 80-bit x87 float (x86-64 System V, passed in memory and returned in st(0)) or an IEEE binary128 (AArch64 Linux), and Rust has no such type at all. A representation which works on some targets and does not exist on others is not one autocxx can offer, so the signature is turned down everywhere instead. Declaring the C++ function in terms of `double`, or adding a C++ wrapper which converts, is the way across. A `long double` *field* is unaffected: it is bytes the struct carries and Rust never reads.")]
    LongDouble,
    #[error("This signature mentions C++'s `__float128`, a 128-bit floating-point type Rust has no equivalent for: `f128` is not a stable Rust type, and the 128-bit type Rust does have, `u128`, is an integer - the same bits read as a different kind of number. bindgen substitutes that `u128` because it is the right size, and autocxx will not put it in a signature, because the shim would compile and the arithmetic on either side of it would be wrong. `unsigned __int128`, which arrives as the same `u128` token and really is that integer, is supported as `autocxx::c_u128`. Declaring the C++ function in terms of `double`, or adding a C++ wrapper which converts, is the way across. A `__float128` *field* is unaffected: it is bytes the struct carries and Rust never reads.")]
    Float128,
    #[error("Encountered static data whose type autocxx can't represent - only variables of POD type, or of a type which bindgen expresses directly in Rust, are supported")]
    StaticDataOfUnsupportedType,
    #[error("The C++ variable {0} has internal linkage, so there is no symbol for Rust to link against. (A namespace-scope variable declared `static`, or declared `const` without `extern`, or declared in an anonymous namespace, exists separately in each translation unit which includes the header.) Declare it `extern` and define it in exactly one C++ file if you want to use it from Rust.")]
    StaticDataWithInternalLinkage(String),
    #[error("Encountered static data of type {}, which autocxx does not expose as something Rust can hold by value. A C++ variable can only be re-exported if its type is POD; try generate_pod! if that type really is trivial.", .0.to_cpp_name())]
    StaticDataOfNonPodType(QualifiedName),
    #[error("The public C++ data member {0} is {1}, so autocxx generates no accessor for it. Add a C++ member function which returns something autocxx can express, and use that instead.")]
    UnrepresentableDataMember(String, UnrepresentableMember),
    #[error("The public C++ data member {0} is of type {}, which is not on the allowlist, so autocxx generates no accessor for it. autocxx generates no methods for a type it was not asked for, and hands out no references to one either; name it in a generate! or generate_pod! directive if you want to read this member.", .1.to_cpp_name())]
    DataMemberOfNonAllowlistedType(String, QualifiedName),
    #[error("autocxx does not know what C++ calls the variable {}, and a variable of non-POD type is reached through a getter whose C++ has to name it. bindgen reports no C++ spelling for a variable, and the name it gave this one is not the one in the mangled symbol - which is so for a static data member of a class, whose name bindgen flattens into the enclosing namespace. A member of POD type is unaffected, being re-exported through bindgen's own declaration. Add a static member function which returns the object, and bind that.", .0.to_cpp_name())]
    StaticDataWithUnknownCppName(QualifiedName),
    #[error("Encountered a variable of type {}, which is not one autocxx can hand back. A variable of non-POD type is reached through a getter returning an opaque holder which refers to it, and the holder's accessor has to name the type in the cxx::bridge - which only declares the types autocxx generates. bindgen records a variable's type as a bare name, so a name with template arguments arrives without them and an alias arrives as itself; neither is a type the bridge has declared. Name the type itself in a generate! directive and declare the variable with that type.", .0.to_cpp_name())]
    StaticDataOfUnholdableType(QualifiedName),
    #[error("The variable {} has non-POD type, so it is reached through a getter returning an opaque holder over a std::reference_wrapper - and this header already made autocxx generate that same specialization for something else, where it stands for a C++ type rather than for one of our holders. autocxx will not give one C++ type two meanings. Write the variable's type in a generate! directive and reach it another way.", .0.to_cpp_name())]
    StaticDataHolderAlreadyTaken(QualifiedName),
    #[error("Encountered typedef to itself - this is a known bindgen bug: {0}")]
    InfinitelyRecursiveTypedef(QualifiedName),
    #[error("Unexpected 'use' statement encountered: {}", .0.as_ref().map(|s| s.as_str()).unwrap_or("<unknown>"))]
    UnexpectedUseStatement(Option<String>),
    #[error("Type {} was parameterized over something complex which we don't yet support", .0.to_cpp_name())]
    TemplatedTypeContainingNonPathArg(QualifiedName),
    #[error("Pointer pointed to an array, which is not yet supported")]
    InvalidArrayPointee,
    #[error("This function or method keeps a C++ array in its signature, behind a reference or a pointer ({0}). cxx writes a Rust '[T; N]' as 'std::array<T, N>', which is a different C++ type from the 'T[N]' this was, so the bridge would declare a signature C++ does not have - and the two are spelled the same here, so autocxx cannot tell a 'const T (&)[N]' from a 'const std::array<T, N>&'. An array parameter which C++ decays to a pointer is unaffected: that arrives as a pointer and is bound as one. A 'std::array' passed or returned by value is unaffected too: no C++ function does that with an array, so there is nothing to confuse it with.")]
    CppArrayInSignature(String),
    #[error("Type {0} is a template instantiation with a C++ array among its arguments. autocxx names such an instantiation in C++ by writing its arguments out again, and it cannot tell a 'T[N]' argument from a 'std::array<T, N>' one - both reach it as a Rust '[T; N]' - so the name it wrote would be a specialization the header never made.")]
    CppArrayInTemplateArgument(String),
    #[error("This function or method has a 'std::array' whose element cxx will not hold in one ({0}). cxx spells a Rust '[T; N]' as 'std::array<T, N>' and moves it whole, so the element has to be a type cxx holds by value with no indirection - one of its own atoms, which for a type written in a C++ header means uint8_t or int8_t, a float or a double, a bool, or a char. A class is not one of those, and neither is an integer whose width the platform chooses - which includes 'int' and 'unsigned' and so includes the typedefs to them, 'uint32_t' and 'size_t' among them, because those reach Rust as an 'autocxx::c_*' newtype rather than as a Rust integer.")]
    CppArrayElementNotSupported(String),
    #[error("Pointer pointed to another pointer, which is not yet supported")]
    InvalidPointerPointee,
    #[error("Pointer pointed to something unsupported (autocxx only supports pointers to named types): {0}")]
    InvalidPointee(String),
    #[error("The 'generate' or 'generate_pod' directive for '{0}' did not result in any code being generated. Perhaps this was mis-spelled or you didn't qualify the name with any namespaces? Otherwise please report a bug.")]
    DidNotGenerateAnything(String),
    #[error("The 'generate' or 'generate_pod' directive for '{0}' did not result in any usable code being generated, because autocxx couldn't generate bindings for it: {1}")]
    DidNotGenerateAnythingUsable(String, Box<ConvertErrorFromCpp>),
    #[error("The 'derive' directive for '{0}' matched nothing autocxx generated. Perhaps this was mis-spelled, or you didn't qualify the name with any namespaces, or there is no 'generate' directive for it?")]
    DeriveDirectiveMatchedNothing(String),
    #[error("The 'derive' directive for '{0}' names a type autocxx does not hold by value, so there is no Rust definition of it for the traits to go on. autocxx emits an opaque type with no fields for such a type; use 'generate_pod' if it is safe to hold by value.")]
    DeriveOnTypeWithNoRustDefinition(String),
    #[error("The 'derive' directive asks '{0}' to derive Default, and it is an enum. Nothing makes one enumerator of a C++ enum the default, so Rust would refuse to derive it.")]
    DeriveDefaultOnEnum(String),
    #[error("Found an attempt at using a forward declaration ({}) inside a templated cxx type such as UniquePtr or CxxVector. If the forward declaration is a typedef, perhaps autocxx wasn't sure whether or not it involved a forward declaration. If you're sure it didn't, then you may be able to solve this by using instantiable!.", .0.to_cpp_name())]
    TypeContainingForwardDeclaration(QualifiedName),
    /// Reported in place of [`Self::TypeContainingForwardDeclaration`] where we
    /// know why the stand-in type is standing in. That message suggests
    /// `instantiable!`, which is the answer when autocxx merely couldn't tell
    /// whether a typedef reached a forward declaration - and is no help at all
    /// when the target was, say, a nested type which isn't public. Where
    /// there's a real reason on file, give that.
    #[error("Found an attempt at using {}, which autocxx replaced with an opaque type{}: {}", .name.to_cpp_name(), culprit_clause(.name, .culprit), .reason)]
    TypeContainingUngeneratableTypedef {
        name: QualifiedName,
        culprit: QualifiedName,
        reason: Box<ConvertErrorFromCpp>,
    },
    /// A template instantiation on a type the header only declares. The
    /// instantiation itself is a complete type, so C++ is happy to name one
    /// and to pass references and pointers to it around; but instantiating its
    /// destructor may need the argument to be complete - `au<bb>` holding a
    /// `std::unique_ptr<bb>` is the shape google/autocxx#1065 reported - and
    /// nothing autocxx sees says whether it does. Every position which would
    /// make a C++ compiler instantiate that destructor is turned down here.
    #[error("Found an attempt at using {}, a template instantiation whose argument {} is a type this header only declares. Naming one of these is fine, and so is holding a reference or a pointer to one - but a cxx container of it, or a by-value use, makes C++ destroy one, and destroying a template instantiation can need its argument to be complete. Define {} where autocxx can see it if you need this position.", .instantiation.to_cpp_name(), .argument.to_cpp_name(), .argument.to_cpp_name())]
    InstantiationOnIncompleteType {
        instantiation: QualifiedName,
        argument: QualifiedName,
    },
    #[error("Found an attempt at using a type marked as blocked! ({})", .0.to_cpp_name())]
    Blocked(QualifiedName),
    #[error("This function or method uses a type where one of the template parameters was incomprehensible to bindgen/autocxx - probably because it uses template specialization.")]
    UnusedTemplateParam,
    #[error("This is a C++ alias template: C++ declared {declared} template parameter(s) for it, of which {type_params} are type parameters. bindgen represents no other kind, so it emitted the alias as a plain typedef - but naming the alias in C++ requires the template arguments that typedef has lost, so autocxx cannot generate C++ which uses it. Naming the type the alias points at in a 'generate!' directive is usually what was wanted.")]
    AliasTemplate { declared: usize, type_params: usize },
    #[error("{}", STD_FUNCTION_ADVICE)]
    UnsupportedStdFunction,
    #[error("bindgen could not name this C++ type, and replaced it with an opaque blob of bytes ({0}) of the same size and alignment. autocxx will not put that blob into the bindings in place of the type, because the result would compile and be the wrong signature. The usual two causes are a type C++ only reaches through a `using` declaration bindgen cannot follow - one written in a class, one naming a template, or one whose name two declarations answer to, since a namespace-scope declaration of a type is followed - for which the way out is for the header to spell the type through the namespace which declares it, and std::function; any other type bindgen could not name arrives the same way. {}", STD_FUNCTION_ADVICE)]
    BindgenOpaqueBlob(String),
    #[error("This item relies on a type not known to autocxx ({})", .0.to_cpp_name())]
    UnknownDependentType(QualifiedName),
    #[error("This item depends on some other type(s) which autocxx could not generate, some of them are: {}. {} could not be generated because: {}", .deps.iter().join(", "), .culprit, .reason)]
    IgnoredDependent {
        deps: HashSet<QualifiedName>,
        /// The one of `deps` whose own failure `reason` explains. Naming it
        /// matters when there are several: the reason belongs to this one.
        culprit: QualifiedName,
        /// Why `culprit` could not be generated. This is the original problem,
        /// not another `IgnoredDependent`: an item discarded for depending on
        /// something already discarded inherits that item's reason, so the
        /// message a user reads always names the thing that actually went
        /// wrong rather than a chain of items which merely depended on it.
        reason: Box<ConvertErrorFromCpp>,
    },
    #[error(transparent)]
    InvalidIdent(InvalidIdentError),
    #[error("This item name is used in multiple namespaces. At present, autocxx and cxx allow only one type of a given name. This limitation will be fixed in future. (Items found with this name: {})", .0.iter().join(", "))]
    DuplicateCxxBridgeName(Vec<String>),
    #[error("This is a method on a type which can't be used as the receiver in Rust (i.e. self/this). This is probably because some type involves template specialization.")]
    UnsupportedReceiver,
    #[error("A rust::Box<T> was encountered where T was not known to be a Rust type. Use rust_type!(T): {}", .0.to_cpp_name())]
    BoxContainingNonRustType(QualifiedName),
    #[error("A qualified Rust type was found (i.e. one containing ::): {}. Rust types must always be a simple identifier.", .0.to_cpp_name())]
    RustTypeWithAPath(QualifiedName),
    #[error("This type is nested within another struct/class, yet is abstract (or is not on the allowlist so we can't be sure). This is not yet supported by autocxx. If you don't believe this type is abstract, add it to the allowlist.")]
    AbstractNestedType,
    #[error("This typedef was nested within another struct/class. autocxx is unable to represent inner types if they might be abstract. Unfortunately, autocxx couldn't prove that this type isn't abstract, so it can't represent it.")]
    NestedOpaqueTypedef,
    #[error(
        "This type is nested within another struct/class with protected or private visibility."
    )]
    NonPublicNestedType,
    #[error("A mutable C++ reference to rust::Str appears here. cxx spells Rust's `&str` as a `rust::Str` value, so `rust::Str&` would become `Pin<&mut &str>` in Rust: C++ owns that slot and may write a (pointer, length) pair of its own into it, leaving Rust holding a `&str` whose lifetime nothing has checked. As a parameter, take the `rust::Str` by value - that is how cxx hands a `&str` across - or write `const rust::Str&` if C++ only reads it; the `safety!(unsafe_references_wrapped)` policy accepts the mutable reference as well, wrapping it in a C++ reference type out of which a `&str` can only be got by an unsafe call the caller vouches for. As a return, a borrowed string needs some input reference for autocxx to give it a lifetime, and without one a `rust::Str` or `const rust::Str&` return is turned down too, so return an owned `rust::String`.")]
    MutableReferenceToRustStr,
    #[error("This function returns an rvalue reference (&&) which is not yet supported.")]
    RValueReturn,
    #[error("This method is rvalue-reference-qualified (`&&`), so it can only be called on an object which is about to be discarded. autocxx always holds C++ objects behind a reference or a smart pointer, so it has no way to express that; the method is therefore not generated. See https://github.com/google/autocxx/issues/837.")]
    RValueRefQualifiedMethod,
    #[error("This method is private")]
    PrivateMethod,
    #[error("This type's C++ destructor is inaccessible (private, protected or deleted), so Rust could never destroy one of these. autocxx therefore does not generate constructors, copy/move support or smart pointer support for it; you can still call its methods on a reference or pointer obtained from C++. See https://github.com/google/autocxx/issues/829.")]
    DestructorInaccessible,
    #[error("autocxx does not know how to generate bindings to operator=")]
    AssignmentOperator,
    #[error("This function was marked =delete")]
    Deleted,
    #[error("This special member function was declared =default, but the C++ rules define it as deleted - a member or base has no accessible version of it, or the class has a const or reference member with no initializer. See https://github.com/google/autocxx/issues/815.")]
    DefaultedButDeleted,
    #[error("This structure has an rvalue reference field (&&) which is not yet supported.")]
    RValueReferenceField,
    #[error("A function pointer appears in this signature. cxx has no function pointer type, so autocxx has no way to declare one to it; a function pointer can only be held as struct field data, where it is copied about rather than crossing the language boundary. For C++ to call back into Rust, subclass a C++ observer class from Rust or hand C++ a named Rust function with extern_rust_function - both are described at https://google.github.io/autocxx/rust_calls.html. See https://github.com/google/autocxx/issues/1494.")]
    FunctionPointerInSignature,
    #[error("This type was not on the allowlist, so we are not generating methods for it.")]
    MethodOfNonAllowlistedType,
    #[error("This type is templated, so we can't generate bindings. We will instead generate bindings for each instantiation.")]
    MethodOfGenericType,
    #[error("bindgen generated multiple different APIs (functions/types) with this name. autocxx doesn't know how to disambiguate them, so we won't generate bindings for any of them.")]
    DuplicateItemsFoundInParsing,
    #[error("C++ declares a type of this name in the same scope, which hides this function. Only one of the two can keep the name in the bindings we generate, and it has to be the type, because other bindings may depend on it. Rename the function in C++ if you need to call it from Rust.")]
    FunctionHiddenByType,
    #[error(
        "bindgen generated a move or copy constructor with an unexpected number of parameters."
    )]
    ConstructorWithOnlyOneParam,
    #[error("A copy or move constructor was found to take extra parameters. These are likely to be parameters with defaults, which are not yet supported by autocxx, so this constructor has been ignored.")]
    ConstructorWithMultipleParams,
    #[error("A C++ unique_ptr, shared_ptr or weak_ptr was found containing a type which cannot go in that position ({}): either cxx does not accept it there - the three differ, so a shared_ptr may take what a unique_ptr will not - or it is one of the three autocxx wrappers for which this crate ships no cxx container glue: `autocxx::c_i128` and `autocxx::c_u128`, because MSVC has no `__int128`, and `autocxx::c_char8_t`, because `char8_t` is a C++20 keyword and the glue compiles at C++14.", .0.to_cpp_name())]
    InvalidTypeForCppPtr(QualifiedName),
    #[error("A C++ std::vector was found containing a type which cannot be a vector element ({}): either cxx does not accept it there, or it is one of the three autocxx wrappers for which this crate ships no cxx container glue: `autocxx::c_i128` and `autocxx::c_u128`, because MSVC has no `__int128`, and `autocxx::c_char8_t`, because `char8_t` is a C++20 keyword and the glue compiles at C++14.", .0.to_cpp_name())]
    InvalidTypeForCppVector(QualifiedName),
    #[error("A C++ {} was found whose payload C++ qualified `const`. cxx names a container's payload as a plain type, with nowhere to put the qualifier, so the only thing autocxx could declare is a container of a mutable payload - a different C++ type. std::shared_ptr, std::unique_ptr and std::weak_ptr are lowered to an opaque C++ holder instead; this container is not.", .0.to_cpp_name())]
    ConstCxxContainerPayload(QualifiedName),
    #[error("Variadic functions are not supported by cxx or autocxx.")]
    Variadic,
    #[error("A type had a template inside a std::vector, which is not supported.")]
    GenericsWithinVector,
    #[error("This typedef takes generic parameters, not yet supported by autocxx.")]
    TypedefTakesGenericParameters,
    #[error("This method belonged to an item in an anonymous namespace, not currently supported.")]
    MethodInAnonymousNamespace,
    #[error("We're unable to make a concrete version of this template, because we found an error handling the template.")]
    ConcreteVersionOfIgnoredTemplate,
    #[error("This is a typedef to a type in an anonymous namespace, not currently supported.")]
    TypedefToTypeInAnonymousNamespace,
    #[error("This type refers to a generic type parameter of an outer type, which is not yet supported.")]
    ReferringToGenericTypeParam,
    #[error("This forward declaration was nested within another struct/class. autocxx is unable to represent inner types if they are forward declarations.")]
    ForwardDeclaredNestedType,
    #[error("Problem handling function argument {arg}: {err}")]
    Argument {
        arg: String,
        #[source]
        err: Box<ConvertErrorFromCpp>,
    },
    #[error("autocxx could not tell what C++ reference this function returns. A reference return reaches the code which builds the conversion either as a Rust reference or as the raw pointer the type converter makes of one, and this was neither - it was `{0}`. This is an autocxx bug rather than anything wrong with the C++; please report the header which produced it.")]
    UnexpectedReferenceReturn(String),
    #[error("autocxx has to hand this parameter to the cxx::bridge as a raw pointer, and the type converter rendered it as `{0}` instead. This is an autocxx bug rather than anything wrong with the C++; please report the header which produced it.")]
    ParameterWasNotAPointer(String),
    #[error("autocxx builds a C++ subclass peer by inverting each of the conversions the ordinary wrapper performs, and one of this function's has no opposite to invert to. This is an autocxx bug rather than anything wrong with the C++; please report the header which produced it.")]
    NonInvertibleConversion,
}

/// The kind of C++ data member for which autocxx generates no accessor, named
/// in [`ConvertErrorFromCpp::UnrepresentableDataMember`].
///
/// Each of these is refused where the accessor is synthesized rather than left
/// to the conversion machinery, because what the machinery would write is C++
/// which does not compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnrepresentableMember {
    /// C++ cannot return an array by value, and the reference to one it can
    /// return is not something the bridge spells - cxx writes a Rust `[T; N]`
    /// as `std::array<T, N>`, which is a different C++ type.
    Array,
    /// A reference member. Reading one through the accessor's own reference to
    /// the object is an indirection nothing in the bridge spells, and whether
    /// the result should be borrowed from the object or from what the member
    /// refers to is a question C++ does not answer.
    Reference,
    /// Anything else the type converter made of the member which an accessor's
    /// return type cannot be written around. It makes a field's type a path, a
    /// pointer, an array or a reference, so this is what a fourth kind would
    /// be answered with rather than something a reader has to check for.
    Unrepresentable,
}

impl std::fmt::Display for UnrepresentableMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Array => "an array",
            Self::Reference => "a reference",
            Self::Unrepresentable => "of a kind autocxx has no accessor shape for",
        })
    }
}

/// Which type a [`ConvertErrorFromCpp::TypeContainingUngeneratableTypedef`]
/// should blame, if it is not the typedef the reader is already looking at.
///
/// Usually the two differ and naming the culprit is the whole point. They are
/// the same where bindgen erased the type a typedef named: the alias is then
/// the only name that type has, and "could not generate `Alias`" straight
/// after "using `Alias`" would say nothing twice.
fn culprit_clause(name: &QualifiedName, culprit: &QualifiedName) -> String {
    if name == culprit {
        String::new()
    } else {
        format!(" because it could not generate {}", culprit.to_cpp_name())
    }
}

/// Error types derived from Rust code. This is separate from [`ConvertError`] because these
/// may have spans attached for better diagnostics.
#[derive(Debug, Clone, Error)]
pub enum ConvertErrorFromRust {
    #[error("extern_rust_function only supports limited parameter and return types. This is not such a supported type")]
    UnsupportedTypeForExternFun,
    #[error("extern_rust_function requires a fully qualified receiver, that is: fn a(self: &SomeType) as opposed to fn a(&self)")]
    ExternRustFunRequiresFullyQualifiedReceiver,
    #[error("extern_rust_function cannot support &mut T references; instead use Pin<&mut T> (see cxx documentation for more details")]
    PinnedReferencesRequiredForExternFun,
    #[error("extern_rust_function cannot currently support qualified type paths (that is, foo::bar::Baz). All type paths must be within the current module, imported using 'use'. This restriction may be lifted in future.")]
    NamespacesNotSupportedForExternFun,
    #[error("extern_rust_function signatures must never reference Self: instead, spell out the type explicitly.")]
    ExplicitSelf,
}

/// A [`ConvertErrorFromRust`] which also implements [`miette::Diagnostic`] so can be pretty-printed
/// to show the affected span of code.
#[derive(Error, Debug, Diagnostic, Clone)]
#[error("{err}")]
pub struct LocatedConvertErrorFromRust {
    err: ConvertErrorFromRust,
    #[source_code]
    file: String,
    #[label("error here")]
    span: SourceSpan,
}

impl LocatedConvertErrorFromRust {
    pub(crate) fn new(err: ConvertErrorFromRust, span: &Span, file: &str) -> Self {
        Self {
            err,
            span: proc_macro_span_to_miette_span(span),
            file: file.to_string(),
        }
    }
}

/// Ensures that error contexts are always created using the constructors in this
/// mod, therefore undergoing identifier sanitation.
#[derive(Clone, Debug)]
struct PhantomSanitized;

/// The context of an error, e.g. whether it applies to a function or a method.
/// This is used to generate suitable rustdoc in the output codegen so that
/// the errors can be revealed in rust-analyzer-based IDEs, etc.
#[derive(Clone, Debug)]
pub(crate) struct ErrorContext(Box<ErrorContextType>, PhantomSanitized);

/// The idents in this structure are sanitized against the names autocxx
/// builds in, but not against Rust itself: a name bindgen gave an item can
/// still be a word Rust reserves. Ask [`ErrorContextType::is_declarable`]
/// before generating anything under one of them.
#[derive(Clone, Debug)]
pub(crate) enum ErrorContextType {
    Item(Ident),
    /// An item whose name we had to change because generating code under
    /// its real name wouldn't be safe - it collides with a type autocxx
    /// builds in, such as a C++ function called `Pin`.
    SanitizedItem {
        /// The name the user knows this item by, which is what their
        /// `generate!` directives have to be matched against. Never
        /// appears in generated code.
        lookup: Ident,
        /// The safe name, used for the documentation stub we generate.
        display: Ident,
    },
    Method {
        self_ty: Ident,
        method: Ident,
    },
}

impl ErrorContextType {
    /// Whether Rust would accept the names in here as the names of the items
    /// a documentation stub is made of.
    ///
    /// bindgen names an item `_` when it exists only for its side effect -
    /// the `const _: () = ...` blocks holding its layout assertions are the
    /// case which reaches autocxx. `_` is one of the words Rust reserves, and
    /// a stub declared under a reserved word doesn't parse; the engine used
    /// to panic building one. `_` in particular can't be rescued by a raw
    /// identifier either, and nothing in autocxx writes one, so for such an
    /// item there is no stub to be had and the caller emits nothing at all.
    pub(crate) fn is_declarable(&self) -> bool {
        let declarable = |id: &Ident| validate_str_ok_for_rust(&id.to_string()).is_ok();
        match self {
            Self::Item(id) | Self::SanitizedItem { display: id, .. } => declarable(id),
            Self::Method { self_ty, method } => declarable(self_ty) && declarable(method),
        }
    }
}

impl ErrorContext {
    pub(crate) fn new_for_item(id: Ident) -> Self {
        match Self::sanitize_error_ident(&id) {
            None => Self(Box::new(ErrorContextType::Item(id)), PhantomSanitized),
            Some(display) => Self(
                Box::new(ErrorContextType::SanitizedItem {
                    lookup: id,
                    display,
                }),
                PhantomSanitized,
            ),
        }
    }

    /// An item which lost its own name to something else we kept - a C++
    /// function hidden by a type of the same name, say. The stub goes under
    /// `display`, which the caller has established nothing else has claimed,
    /// since the real name now belongs to whatever won it; the user still
    /// knows the item as `lookup`, which is the name they wrote.
    pub(crate) fn new_for_displaced_item(lookup: Ident, display: Ident) -> Self {
        // Both names are built by appending to an identifier we already know
        // to be legal, which keeps them legal and takes them out of reach of
        // any built-in type name, so `sanitize_error_ident` has nothing to do.
        Self(
            Box::new(ErrorContextType::SanitizedItem { lookup, display }),
            PhantomSanitized,
        )
    }

    pub(crate) fn new_for_method(self_ty: Ident, method: Ident) -> Self {
        // If this IgnoredItem relates to a method on a self_ty which we can't represent,
        // e.g. u8, then forget about trying to attach this error text to something within
        // an impl block.
        match Self::sanitize_error_ident(&self_ty) {
            None => Self(
                Box::new(ErrorContextType::Method {
                    self_ty,
                    method: Self::sanitize_error_ident(&method).unwrap_or(method),
                }),
                PhantomSanitized,
            ),
            // The stub becomes a free item rather than a method, but the
            // thing the user asked us to generate is still the type, so
            // that remains how we look this error up.
            Some(_) => Self(
                Box::new(ErrorContextType::SanitizedItem {
                    display: make_ident(format!("{self_ty}_{method}")),
                    lookup: self_ty,
                }),
                PhantomSanitized,
            ),
        }
    }

    /// Because errors may be generated for invalid types or identifiers,
    /// we may need to scrub the name
    fn sanitize_error_ident(id: &Ident) -> Option<Ident> {
        let qn = QualifiedName::new(&Namespace::new(), id.clone());
        if known_types().conflicts_with_built_in_type(&qn) {
            Some(make_ident(format!("{}_autocxx_error", qn.get_final_item())))
        } else {
            None
        }
    }

    pub(crate) fn get_type(&self) -> &ErrorContextType {
        &self.0
    }

    pub(crate) fn into_type(self) -> ErrorContextType {
        *self.0
    }
}

impl std::fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            ErrorContextType::Item(id) | ErrorContextType::SanitizedItem { display: id, .. } => {
                write!(f, "{id}")
            }
            ErrorContextType::Method { self_ty, method } => write!(f, "{self_ty}::{method}"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ConvertErrorWithContext(
    pub(crate) ConvertErrorFromCpp,
    pub(crate) Option<ErrorContext>,
);

impl std::fmt::Debug for ConvertErrorWithContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for ConvertErrorWithContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::ConvertErrorFromCpp;
    use crate::types::QualifiedName;
    use indexmap::set::IndexSet as HashSet;

    /// The chain a user reads on MSVC when a function takes a `std::function`,
    /// as observed in CI:
    ///
    /// ```text
    /// DidNotGenerateAnythingUsable("takes_callback", IgnoredDependent {
    ///     deps: {std::function}, culprit: std::function,
    ///     reason: UnsupportedStdFunction })
    /// ```
    ///
    /// No test on a libstdc++ or libc++ host can produce that - there
    /// std::function is erased before autocxx sees the name - so this assembles
    /// it by hand and checks the advice survives both wrappers, which is what
    /// `test_std_function_parameter_says_what_went_wrong` asserts.
    #[test]
    fn the_std_function_advice_survives_the_msvc_error_chain() {
        let std_function = QualifiedName::new_from_cpp_name("std::function");
        let mut deps = HashSet::new();
        deps.insert(std_function.clone());
        let err = ConvertErrorFromCpp::DidNotGenerateAnythingUsable(
            "takes_callback".to_string(),
            Box::new(ConvertErrorFromCpp::IgnoredDependent {
                deps,
                culprit: std_function,
                reason: Box::new(ConvertErrorFromCpp::UnsupportedStdFunction),
            }),
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("std::function is not supported by bindgen or cxx"),
            "the advice did not reach the top of the chain: {rendered}"
        );
    }

    /// The other chain MSVC produces, when the `std::function` is reached
    /// through a class-scoped `using` alias rather than named outright. Same
    /// advice, one more wrapper. Assembled by hand for the same reason as
    /// above; what exercises it end to end is
    /// `test_class_scoped_alias_reports_the_targets_own_problem`, which builds
    /// the same shape out of a private nested class so that it runs anywhere.
    #[test]
    fn the_std_function_advice_survives_the_hop_through_an_alias() {
        let err = ConvertErrorFromCpp::TypeContainingUngeneratableTypedef {
            name: QualifiedName::new_from_cpp_name("Requester::RespHandler"),
            culprit: QualifiedName::new_from_cpp_name("std::function"),
            reason: Box::new(ConvertErrorFromCpp::UnsupportedStdFunction),
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("std::function is not supported by bindgen or cxx"),
            "the advice did not survive the alias: {rendered}"
        );
        assert!(
            rendered.contains("RespHandler"),
            "the alias the user wrote went unnamed: {rendered}"
        );
    }
}
