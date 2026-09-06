// Copyright 2026 The autocxx maintainers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Repairs the bindgen-generated mod, which autocxx emits verbatim,
//! in the cases where bindgen hands us Rust that could never compile.
//!
//! # Unbound template parameters
//!
//! With some standard library headers (particularly newer libc++,
//! e.g. Xcode 26), bindgen emits type aliases for class-scoped
//! typedefs where the right-hand side still names a template
//! parameter of the enclosing C++ class, but the alias itself
//! declares no such generic parameter:
//!
//! ```text
//! pub type basic_string___self_view = root::std::basic_string_view<_CharT>;
//! ```
//!
//! `_CharT` is not declared anywhere, so the generated code fails
//! with E0425 "cannot find type `_CharT` in this scope". See
//! google/autocxx#1480 and google/autocxx#1051. Since autocxx
//! includes bindgen's output verbatim, we must prune such items
//! before emitting the mod.
//!
//! Removal is safe for anything that referenced a pruned alias by
//! that bare name: it could not have compiled either, and alias
//! chains are handled by pruning to a fixpoint. Known limitation:
//! a struct *field* typed via a multi-segment path to a pruned
//! alias (e.g. `root::std::__tree___end_node_t`) is left dangling —
//! the struct was equally uncompilable before, but fixing it needs
//! a cascade to opaque the containing struct, tracked separately.
//!
//! # Colliding names
//!
//! bindgen names a member of a class by joining the names of its
//! ancestors, so `Outer::Inner` becomes `Outer_Inner` and two such
//! `Inner`s never collide. That breaks down for members of a class
//! *template specialization*: bindgen does not record the
//! specialization as the member's parent, so the member is emitted
//! into an enclosing module under its own bare name. Two distinct
//! C++ types then land on one Rust name:
//!
//! ```text
//! pub type iterator = root::pointer;      // absl::cj<l>::iterator
//! pub struct iterator { _unused: [u8; 0] } // absl::j::ct<..>::iterator
//! ```
//!
//! which is E0428, "the name `iterator` is defined multiple times".
//! See google/autocxx#490. (Not to be confused with
//! google/autocxx#486, where two types from *different* namespaces
//! collide in the flat `cxx::bridge` namespace; `check_names` catches
//! that one and reports it.)
//!
//! We cannot repair this by renaming. Every reference bindgen emitted
//! (`root::iterator` in a field, in a type alias, in an `extern "C"`
//! signature) names whichever of the two types the C++ meant, and
//! bindgen's output no longer records which — so renaming one of them
//! would have to guess at each reference site, and guessing wrong
//! silently rewires an FFI signature to the wrong type. Instead we
//! collapse the collision into a single opaque placeholder, so that
//! every reference still resolves and the mod compiles.
//!
//! That is only sound for a name the *parse phase* recorded as
//! duplicated, which is why the caller passes that set in rather than
//! letting us infer it from the emitted mod. For such a name,
//! `ApiVec::push` has already replaced every API with an
//! `Api::IgnoredItem`, so nothing depending on it can be generated and
//! the references left behind are debris. A name which merely *looks*
//! duplicated carries no such guarantee: the parse phase skips some
//! declarations (an unrepresentable struct, say) while a type alias of
//! the same name survives, is re-exported verbatim, and may be
//! genuinely referenced. Collapsing that would quietly turn an alias
//! of a pointer into a zero-sized struct, so we leave it alone.
//!
//! # Unwanted `Default`
//!
//! We ask bindgen to `derive_default`, because a struct containing
//! bitfields cannot otherwise be built in Rust at all: neither the
//! allocation unit nor the padding beside it can be written by hand.
//! For a struct whose fields are all default-able that is exactly what
//! we want - bindgen adds `#[derive(Default)]` and every field
//! supplies its own default. It also has two consequences we don't
//! want, and this pass withdraws both.
//!
//! ## On an `enum`
//!
//! Nothing makes one enumerator of a C++ enum the default, so autocxx
//! should never offer `Default` for one - and Rust agrees: a
//! `#[derive(Default)]` on an enum with no variant marked `#[default]`
//! is E0665, a hard error. bindgen marks no variant, and its enum
//! codegen puts `Default` in the derive list whenever `derive_default`
//! is on and its analysis says the item can derive it, so the mod
//! simply fails to compile. That is what happened to
//! `_Rb_tree__bindgen_ty_1`, an anonymous enum inside libstdc++, once
//! `derive_default` was turned on: invisible on a libc++ box, fatal on
//! CI. Whether any given enum trips it depends on the standard library
//! and libclang in front of us, so we strip `Default` from every enum's
//! derive list rather than reacting to the ones we happen to have seen.
//!
//! ## Zero-filled, on anything
//!
//! Unlike everything above, this one is about code which compiles
//! perfectly well and is unsound.
//!
//! For a type which *cannot* derive `Default`, bindgen instead writes
//! an `impl Default` of its own which zeroes the object's bytes. That
//! is a defensible default for C, and wrong for us: a struct holding a
//! Rust `enum` whose discriminants don't include 0 - precisely the
//! shape `enum_style!(RustifiedEnum, ...)` exists to produce - would
//! hand out an enum value which is none of its variants, which is
//! undefined behaviour, reachable from entirely safe Rust.
//!
//! bindgen offers no switch separating the two: its `no_default`
//! blocklist suppresses the derive as well, being consulted by the
//! derive analysis itself. So we keep the derives and drop the
//! zero-filling impls here. For the types that had one, this restores
//! exactly the previous behaviour: before we turned `derive_default`
//! on, bindgen wrote neither a derive nor an impl for them.
//!
//! "Zero-filling" is meant literally, and is what the pass matches on.
//! Not every hand-written `Default` in the bindgen output is one:
//! bindgen also writes `Default` for its own `__BindgenUnionField` and
//! `__BindgenOpaqueArray` helpers, regardless of `derive_default`, and
//! those construct a value properly rather than zeroing bytes. They
//! are none of our business and are left alone.
//!
//! # Bitfield accessors which transmute
//!
//! A bitfield lives in an allocation unit bindgen represents as a byte
//! array, and its accessors move the bits between that array and the
//! field's own Rust type with `mem::transmute`: the getter reads a
//! `u64` out of the unit, casts it to the unsigned integer of the
//! field's width, and transmutes that to the field's type; the setter
//! and the `new_bitfield_N` constructor go the other way.
//!
//! Where such a transmute is between two scalars that `as` could
//! convert just as well - `u32` to `i32`, `bool` to `u8` - rustc's
//! `unnecessary_transmutes` lint says so. It warns by default, so
//! anyone building the generated code with `-D warnings` cannot
//! compile a struct containing a bitfield at all; that is how it
//! reached us, through `llvm::ErrorOr` in `examples/llvm`.
//!
//! So this pass writes those conversions the way the lint asks for:
//!
//! | field type | getter | setter |
//! |---|---|---|
//! | the unit's own integer | the unit's value unchanged | the value unchanged |
//! | another integer of that width | `as` the field's type | `as` the unit's integer |
//! | `bool` | `!= 0` | `as` the unit's integer |
//!
//! Each is exactly what the transmute did. Two same-width integers
//! differing only in signedness have the same object representation,
//! so an `as` cast between them reinterprets the bits and changes
//! nothing else - and the width is bindgen's own, since it picks the
//! unit's integer from the layout of the field's type. A `bool` is
//! `0` or `1` and nothing else, so `as` produces the byte the
//! transmute would have; back the other way `!= 0` agrees with the
//! transmute on those two values and, unlike it, is not undefined
//! behaviour on any other.
//!
//! The first row covers more than it looks: `unsigned` reaches Rust as
//! `::std::os::raw::c_uint` while the allocation unit deals in `u32`,
//! and those are one type under two names. Casting between them would
//! be `clippy::unnecessary_cast` - this same problem in another lint's
//! clothing - so the pass writes no cast whenever both sides are
//! fixed-width unsigned types, and lets rustc confirm they agree.
//!
//! Which row a field lands in is a question about the type it finally
//! names rather than the name it was declared with, since that is what
//! rustc lints, so the pass follows the mod's own `typedef`s: a field
//! declared `Handle h : 4` is seen for the `int` it is.
//!
//! Anything else keeps its transmute. In particular a bitfield of C++
//! `enum` type transmutes an integer into a Rust enum, which no cast
//! can express - and rustc knows that, which is why the lint leaves it
//! alone too.
//!
//! Dropping the transmute from a getter or setter empties the `unsafe`
//! block around it, and an `unsafe` block with nothing unsafe left in
//! it is `unused_unsafe`: the same problem again, wearing a different
//! lint. So the pass has to decide about that block too, and only does
//! so for accessors it recognizes in full - it has to be able to see
//! what is left:
//!
//! * a bitfield of a struct reads its allocation unit as
//!   `self._bitfield_1.get(..)`, an inherent method of bindgen's
//!   `__BindgenBitfieldUnit` and safe, so the block goes;
//! * a bitfield of a `union` reaches the unit through
//!   `self._bitfield_1.as_ref().get(..)`, and
//!   `__BindgenUnionField::as_ref` is an `unsafe fn`, so the block
//!   stays;
//! * the `_raw` accessors, which are `unsafe fn`s that dereference the
//!   pointer they are handed, keep theirs whatever else they do.
//!
//! An accessor of some other shape is emitted exactly as bindgen wrote
//! it, transmute and all. The `new_bitfield_N` constructor needs none of
//! this reasoning - each field there is converted inside an `unsafe`
//! block holding that transmute and nothing else, so the block goes with
//! the transmute it wrapped - but it is matched just as narrowly, so
//! that the pass only ever reaches the statements documented below.
//!
//! What makes it safe to do all this by matching on syntax, without
//! knowing any of the types involved, is that rustc checks every
//! assumption afterwards. A cast between types of different widths is
//! not a silent truncation here: it is written where bindgen wrote its
//! own type annotation or return type, so a disagreement about the
//! width is a type error. A cast we write for two names of one type is
//! merely redundant. And an `unsafe` block removed from a body that
//! still needed one is E0133, a hard error, not a quiet loss of
//! checking. If a future bindgen writes some different shape, the
//! worst it can do is stop matching, which brings the lint back and
//! fails the test that brought us here.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::{
    parse_quote, Attribute, Block, Expr, FnArg, GenericArgument, GenericParam, Ident, ImplItem,
    ImplItemFn, Item, ItemMod, Local, Pat, Path, PathArguments, ReturnType, Signature, Stmt, Type,
    TypeParamBound, UseTree,
};

use crate::conversion::derives::DeriveRequests;
use crate::types::{make_ident, Namespace, QualifiedName};

/// Remove type aliases in the bindgen mod (recursively) which refer
/// to type names that are not bound anywhere: not a generic parameter
/// of the alias, not a type defined or imported in the bindgen output,
/// and not a Rust primitive.
///
/// Pruning iterates to a fixpoint: removing an alias takes its name
/// out of scope, which can in turn invalidate aliases that referenced
/// it (`type Good = BadAlias;`).
pub(super) fn remove_unbound_type_aliases(bindgen_mod: &mut ItemMod) {
    let mut defined = HashSet::new();
    collect_defined_type_names(bindgen_mod, &mut defined);
    loop {
        let mut pruned = Vec::new();
        prune_mod(bindgen_mod, &defined, &mut pruned);
        if pruned.is_empty() {
            break;
        }
        for name in pruned {
            defined.swap_remove(&name);
        }
    }
}

/// Withdraw the two kinds of `Default` we asked bindgen for by accident,
/// keeping the one we wanted:
///
/// * the `impl Default` blocks bindgen wrote by hand, which zero the object's
///   bytes;
/// * `Default` in the derive list of an `enum`.
///
/// A struct's `#[derive(Default)]` is left alone: it delegates to each field's
/// own `Default`, and is the reason we turn `derive_default` on at all.
///
/// See the module documentation for both rationales.
pub(super) fn remove_unwanted_defaults(bindgen_mod: &mut ItemMod) {
    if let Some((_, items)) = &mut bindgen_mod.content {
        items.retain(|item| !is_zero_filling_default_impl(item));
        for item in items {
            match item {
                Item::Enum(e) => remove_default_from_derives(&mut e.attrs),
                Item::Mod(m) => remove_unwanted_defaults(m),
                _ => {}
            }
        }
    }
}

/// Take `Default` out of any `#[derive(...)]` among `attrs`, dropping the
/// attribute altogether if nothing else was being derived.
///
/// A `derive` we can't parse is left exactly as it was: this is a narrowing of
/// what bindgen asked for, so declining to act is always the safe answer.
fn remove_default_from_derives(attrs: &mut Vec<Attribute>) {
    attrs.retain_mut(|attr| {
        if !attr.path().is_ident("derive") {
            return true;
        }
        let mut kept: Vec<Path> = Vec::new();
        let mut found_default = false;
        let parsed = attr.parse_nested_meta(|meta| {
            if meta
                .path
                .segments
                .last()
                .is_some_and(|seg| seg.ident == "Default")
            {
                found_default = true;
            } else {
                kept.push(meta.path.clone());
            }
            Ok(())
        });
        if parsed.is_err() || !found_default {
            return true;
        }
        if kept.is_empty() {
            return false;
        }
        *attr = parse_quote! { #[derive(#(#kept),*)] };
        true
    });
}

/// Whether `item` is an `impl Default` whose `default` zeroes the object's
/// bytes.
///
/// The test is the body, not the trait name, because the trait name alone
/// catches impls we must keep: bindgen emits `Default` for its own
/// `__BindgenUnionField` (which calls `Self::new()`) and `__BindgenOpaqueArray`
/// (which initializes each element), and does so whether or not
/// `derive_default` was asked for. Neither zeroes anything, and neither is ours
/// to remove.
///
/// Every zero-filling body bindgen generates goes through `ptr::write_bytes` -
/// it uses that rather than `mem::zeroed` so that padding is zeroed too - in
/// one of two shapes depending on whether the target supports `MaybeUninit`.
/// Looking for that call recognizes both, and says in the code what the actual
/// objection is.
fn is_zero_filling_default_impl(item: &Item) -> bool {
    let Item::Impl(imp) = item else {
        return false;
    };
    let Some((None, trait_path, _)) = &imp.trait_ else {
        return false;
    };
    if !trait_path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "Default")
    {
        return false;
    }
    imp.items.iter().any(|item| match item {
        ImplItem::Fn(f) if f.sig.ident == "default" => {
            mentions_ident(f.block.to_token_stream(), "write_bytes")
        }
        _ => false,
    })
}

/// Whether `tokens` mentions `wanted` anywhere, at any nesting depth.
fn mentions_ident(tokens: TokenStream, wanted: &str) -> bool {
    tokens.into_iter().any(|tt| match tt {
        TokenTree::Ident(id) => id == wanted,
        TokenTree::Group(g) => mentions_ident(g.stream(), wanted),
        _ => false,
    })
}

/// Replace the `mem::transmute` in each of bindgen's bitfield accessors with
/// the cast which does the same thing, so that the generated code does not
/// trip `unnecessary_transmutes`.
///
/// Only the exact shapes bindgen writes are matched, and only for the scalar
/// types a cast can convert; anything else is left as bindgen wrote it. See
/// the module documentation.
pub(super) fn simplify_bitfield_transmutes(bindgen_mod: &mut ItemMod) {
    let mut aliases = TypeAliases::default();
    aliases.collect_from(bindgen_mod, &[]);
    simplify_bitfield_transmutes_in_mod(bindgen_mod, &aliases);
}

fn simplify_bitfield_transmutes_in_mod(item_mod: &mut ItemMod, aliases: &TypeAliases) {
    if let Some((_, items)) = &mut item_mod.content {
        for item in items {
            match item {
                // Inherent impls only: the bitfield accessors are inherent
                // methods, and no trait impl bindgen writes has one.
                Item::Impl(imp) if imp.trait_.is_none() => {
                    for impl_item in &mut imp.items {
                        if let ImplItem::Fn(f) = impl_item {
                            simplify_bitfield_accessor(f, aliases);
                        }
                    }
                }
                Item::Mod(m) => simplify_bitfield_transmutes_in_mod(m, aliases),
                _ => {}
            }
        }
    }
}

/// Rewrite whichever of bindgen's three bitfield shapes `f` is, if any.
fn simplify_bitfield_accessor(f: &mut ImplItemFn, aliases: &TypeAliases) {
    if rewrite_bitfield_getter(f, aliases) || rewrite_bitfield_setter(f, aliases) {
        return;
    }
    rewrite_bitfield_unit_constructor(f, aliases);
}

/// The type aliases the bindgen mod declares, so that a bitfield of a
/// `typedef`'d type - `typedef int Handle; struct S { Handle h : 4; };` - can
/// be seen for the integer it is. Without this the pass would decline to touch
/// it and the lint would still fire on the alias's underlying type, which is
/// what rustc sees.
///
/// Keyed by the path a reference to the alias is written with. bindgen writes
/// those from the mod root - `root::Handle`, `root::ns::Handle` - which is
/// exactly the path from the bindgen mod down to where the alias is declared.
#[derive(Default)]
struct TypeAliases(HashMap<Vec<String>, Type>);

impl TypeAliases {
    fn collect_from(&mut self, item_mod: &ItemMod, prefix: &[String]) {
        if let Some((_, items)) = &item_mod.content {
            for item in items {
                match item {
                    // A generic alias needs its arguments substituting to mean
                    // anything, which is more than this is for.
                    Item::Type(t) if t.generics.params.is_empty() => {
                        let mut path = prefix.to_vec();
                        path.push(t.ident.to_string());
                        self.0.insert(path, (*t.ty).clone());
                    }
                    Item::Mod(m) => {
                        let mut path = prefix.to_vec();
                        path.push(m.ident.to_string());
                        self.collect_from(m, &path);
                    }
                    _ => {}
                }
            }
        }
    }

    /// The type `ty` finally names, following it through however many aliases
    /// it takes. A type which is not an alias is returned as it stands.
    ///
    /// The chain is followed to its end rather than to any fixed depth: a
    /// header is free to define as many aliases as it likes, and stopping
    /// early would silently leave the transmute in place and the lint firing.
    /// The only thing that stops the walk short is an alias which leads back
    /// to one already followed - a cycle, which no mod containing it could
    /// compile, so there is nothing there to get right.
    fn resolve<'a>(&'a self, ty: &'a Type) -> &'a Type {
        let mut current = ty;
        let mut followed: HashSet<Vec<String>> = HashSet::new();
        loop {
            let Some(key) = alias_key(current) else {
                return current;
            };
            let Some(next) = self.0.get(&key) else {
                return current;
            };
            if !followed.insert(key) {
                return current;
            }
            current = next;
        }
    }
}

/// The path by which an alias declared in the bindgen mod would be named, if
/// `ty` is written as such a path at all. A leading `::` means an absolute
/// path to somewhere outside the mod - `::std::os::raw::c_int` - and never
/// names one of these.
fn alias_key(ty: &Type) -> Option<Vec<String>> {
    let Type::Path(tp) = ty else {
        return None;
    };
    if tp.qself.is_some() || tp.path.leading_colon.is_some() {
        return None;
    }
    tp.path
        .segments
        .iter()
        .map(|seg| seg.arguments.is_none().then(|| seg.ident.to_string()))
        .collect()
}

/// ```text
/// fn field(&self) -> Ty {
///     unsafe { ::std::mem::transmute(self._bitfield_1.get(0usize, 3u8) as u8) }
/// }
/// ```
fn rewrite_bitfield_getter(f: &mut ImplItemFn, aliases: &TypeAliases) -> bool {
    let ReturnType::Type(_, dest) = &f.sig.output else {
        return false;
    };
    let Some(inner) = sole_unsafe_block(&f.block) else {
        return false;
    };
    let [Stmt::Expr(expr, None)] = &inner.stmts[..] else {
        return false;
    };
    let Some(arg) = transmute_argument(expr) else {
        return false;
    };
    // The `as` to the allocation unit's integer type, which is what tells us
    // what the transmute is converting from.
    let Expr::Cast(cast) = arg else {
        return false;
    };
    let Some(needs_unsafe) = remaining_unsafety(&f.sig, &cast.expr, "get") else {
        return false;
    };
    let Some(converted) = safe_conversion(arg.clone(), &cast.ty, dest, aliases) else {
        return false;
    };
    f.block = accessor_body(needs_unsafe, vec![Stmt::Expr(converted, None)]);
    true
}

/// Whether the accessor's `unsafe` block must stay once the transmute has gone
/// from it, or `None` if this is not an accessor we recognize well enough to
/// say.
///
/// An `unsafe fn` here is one of bindgen's `_raw` accessors, which dereferences
/// the raw pointer it is handed and calls the allocation unit's own `unsafe`
/// `raw_get`/`raw_set`. There is no shape to check: it needs the block whatever
/// else it does.
fn remaining_unsafety(sig: &Signature, unit_access: &Expr, method: &str) -> Option<StillUnsafe> {
    if sig.unsafety.is_some() {
        return Some(StillUnsafe::Yes);
    }
    allocation_unit_access(unit_access, method)
}

/// ```text
/// fn set_field(&mut self, val: Ty) {
///     unsafe {
///         let val: u8 = ::std::mem::transmute(val);
///         self._bitfield_1.set(0usize, 3u8, val as u64)
///     }
/// }
/// ```
fn rewrite_bitfield_setter(f: &mut ImplItemFn, aliases: &TypeAliases) -> bool {
    let Some(inner) = sole_unsafe_block(&f.block) else {
        return false;
    };
    let [Stmt::Local(local), tail @ Stmt::Expr(tail_expr, None)] = &inner.stmts[..] else {
        return false;
    };
    let Some(needs_unsafe) = remaining_unsafety(&f.sig, tail_expr, "set") else {
        return false;
    };
    let Some(rewritten) = rewrite_transmuted_parameter(local, &f.sig, aliases) else {
        return false;
    };
    let stmts = vec![Stmt::Local(rewritten), tail.clone()];
    f.block = accessor_body(needs_unsafe, stmts);
    true
}

/// ```text
/// fn new_bitfield_1(field: Ty) -> __BindgenBitfieldUnit<[u8; 1usize]> {
///     let mut __bindgen_bitfield_unit: ... = Default::default();
///     __bindgen_bitfield_unit.set(0usize, 3u8, {
///         let field: u8 = unsafe { ::std::mem::transmute(field) };
///         field as u64
///     });
///     __bindgen_bitfield_unit
/// }
/// ```
///
/// Each field gets its own `unsafe` block wrapping its own transmute and
/// nothing else, so unlike the accessors above there is no shared block to
/// reason about: rewriting one field's conversion takes that field's `unsafe`
/// with it and leaves the others alone. That is what lets an `enum` field
/// keep its transmute while the integers beside it lose theirs.
///
/// The statement is matched as tightly as the accessors are - a three-argument
/// `set` on the local bindgen declares by name, with a block for its value -
/// so that the pass reaches only the shape documented above.
fn rewrite_bitfield_unit_constructor(f: &mut ImplItemFn, aliases: &TypeAliases) {
    // Destructured so that the signature can be read while the body is being
    // rewritten.
    let ImplItemFn { sig, block, .. } = f;
    for stmt in &mut block.stmts {
        let Stmt::Expr(Expr::MethodCall(call), Some(_)) = stmt else {
            continue;
        };
        if call.method != "set"
            || call.args.len() != 3
            || !is_path_ident(&call.receiver, "__bindgen_bitfield_unit")
        {
            continue;
        }
        let Some(Expr::Block(field)) = call.args.iter_mut().nth(2) else {
            continue;
        };
        for stmt in &mut field.block.stmts {
            let Stmt::Local(local) = stmt else {
                continue;
            };
            if let Some(rewritten) = rewrite_transmuted_parameter(local, sig, aliases) {
                *local = rewritten;
            }
        }
    }
}

/// Given `let val: u8 = ::std::mem::transmute(val);` - optionally with the
/// transmute in an `unsafe` block of its own, as the bitfield unit
/// constructor writes it - the same `let` with the transmute replaced by a
/// cast, and the `unsafe` block gone with it.
///
/// `None` if this is some other `let`, or if the parameter's type is not one
/// a cast can convert.
fn rewrite_transmuted_parameter(
    local: &Local,
    sig: &Signature,
    aliases: &TypeAliases,
) -> Option<Local> {
    let Pat::Type(annotated) = &local.pat else {
        return None;
    };
    let dest = &*annotated.ty;
    let init = local.init.as_ref()?;
    if init.diverge.is_some() {
        return None;
    }
    let transmute = match &*init.expr {
        Expr::Unsafe(block) => match &block.block.stmts[..] {
            [Stmt::Expr(expr, None)] => expr,
            _ => return None,
        },
        expr => expr,
    };
    let arg = transmute_argument(transmute)?;
    // The value being transmuted is a parameter, whose declared type is what
    // the transmute is converting from.
    let source = parameter_type(sig, arg)?;
    let converted = safe_conversion(arg.clone(), source, dest, aliases)?;
    let mut rewritten = local.clone();
    if let Some(init) = &mut rewritten.init {
        *init.expr = converted;
    }
    Some(rewritten)
}

/// The expression which produces exactly what `transmute::<Source, Dest>(expr)`
/// produces, for the scalar pairs a bitfield accessor converts between.
///
/// `None` for any other pair, which leaves bindgen's transmute in place: a
/// bitfield of `enum` type is the case that matters, and there is no cast from
/// an integer to a Rust enum.
///
/// `expr` is always a cast or a bare identifier here, both of which bind more
/// tightly than the operators below, so the result needs no parentheses. The
/// cast is written to `dest` as bindgen spelled it, alias and all, which reads
/// better and means the same thing.
fn safe_conversion(expr: Expr, source: &Type, dest: &Type, aliases: &TypeAliases) -> Option<Expr> {
    // What the types are is a question about what they finally name, not about
    // how the field was declared: rustc lints the underlying type, so a
    // `typedef` of `int` has to be treated as the `int` it is.
    let source = aliases.resolve(source);
    let resolved_dest = aliases.resolve(dest);
    if is_bool_type(resolved_dest) {
        // The transmute reads the allocation unit's byte as a `bool`, which
        // has no meaning for any value but 0 and 1. `!= 0` agrees with it on
        // those and is defined on the rest.
        return integer_kind(source)
            .is_some()
            .then(|| parse_quote! { #expr != 0 });
    }
    let dest_kind = integer_kind(resolved_dest)?;
    if is_bool_type(source) {
        // `as` on a `bool` produces 1 or 0, which is its object
        // representation, which is what the transmute produced.
        return Some(parse_quote! { #expr as #dest });
    }
    let source_kind = integer_kind(source)?;
    // Two integers of the same width have the same object representation
    // whatever their signedness, so an `as` cast between them is exactly the
    // transmute. bindgen guarantees the widths match: it picks the allocation
    // unit's integer type from the layout of the field's own type.
    //
    // Except that half the time no conversion is called for at all, because
    // the two names are the same type - `unsigned` reaches Rust as
    // `::std::os::raw::c_uint` and the allocation unit deals in `u32`. Writing
    // a cast there would be `clippy::unnecessary_cast`, which is the problem
    // we are fixing wearing yet another hat. Since the unit's integer is
    // always one of `u8` to `u128`, any other fixed-width unsigned type of the
    // same width *is* that type; leaving the cast out asks rustc to confirm
    // it, and rustc says so loudly if bindgen ever disagrees with itself about
    // the width.
    let identical = same_type(source, resolved_dest)
        || (source_kind == IntegerKind::FixedUnsigned && dest_kind == IntegerKind::FixedUnsigned);
    if identical {
        return Some(expr);
    }
    Some(parse_quote! { #expr as #dest })
}

/// Whether two types are spelled identically.
fn same_type(a: &Type, b: &Type) -> bool {
    a.to_token_stream().to_string() == b.to_token_stream().to_string()
}

fn is_bool_type(ty: &Type) -> bool {
    match ty {
        Type::Path(tp) => tp.qself.is_none() && tp.path.is_ident("bool"),
        _ => false,
    }
}

/// What we know about an integer type without knowing the target platform.
#[derive(PartialEq, Eq, Clone, Copy)]
enum IntegerKind {
    /// One of `u8` to `u128`, or an alias for one of them.
    FixedUnsigned,
    /// One of `i8` to `i128`, or an alias for one of them.
    FixedSigned,
    /// An integer which is neither: `usize` and `isize`, which are their own
    /// types however wide they turn out to be, and `c_char`, whose signedness
    /// is the platform's business (`u8` on ARM Linux, `i8` on x86-64). Both
    /// need writing out as a cast rather than assuming which fixed-width type
    /// they coincide with.
    ///
    /// For `c_char` that costs a cast which is redundant on the platforms
    /// where `char` is unsigned - `clippy::unnecessary_cast` would say so
    /// about a `char` bitfield built for ARM Linux. Since which platforms
    /// those are is not something this pass can see, the alternative is to
    /// write no cast and fail to compile on the other half of them.
    Other,
}

/// What kind of integer `ty` is: one of Rust's own primitives, or one of the
/// aliases bindgen writes for a plain C type. `None` for anything else,
/// including a C++ `enum` and any type we don't recognize.
///
/// The C aliases are matched against the whole path, absolute leading `::`
/// and all, because that is how bindgen writes them and nothing else can
/// produce one: `codegen::helpers::ast_ty::raw_type` emits
/// `::std::os::raw::c_int`, or `::core::ffi::c_int` for a `use_core` build,
/// and autocxx sets no `ctypes_prefix` which would change that. Anything
/// bindgen derives from the C++ in front of it is a *relative* path under
/// `root`, so a C++ `namespace raw { enum c_int ... }` arrives as
/// `root::raw::c_int` and is none of our business. Recognizing it as an
/// integer would have us emit `x as root::raw::c_int`, which is E0605.
fn integer_kind(ty: &Type) -> Option<IntegerKind> {
    let Type::Path(tp) = ty else {
        return None;
    };
    if tp.qself.is_some() {
        return None;
    }
    // A generic argument anywhere means this is not one of the plain names
    // below, whatever the idents say.
    if tp.path.segments.iter().any(|seg| !seg.arguments.is_none()) {
        return None;
    }
    let idents: Vec<String> = tp
        .path
        .segments
        .iter()
        .map(|seg| seg.ident.to_string())
        .collect();
    let segments: Vec<&str> = idents.iter().map(String::as_str).collect();
    let name = match (tp.path.leading_colon.is_some(), segments.as_slice()) {
        // A Rust primitive is a bare name and nothing else.
        (false, [name]) => *name,
        (true, ["std", "os", "raw", name]) | (true, ["core", "ffi", name]) => *name,
        _ => return None,
    };
    match name {
        "u8" | "u16" | "u32" | "u64" | "u128" | "c_uchar" | "c_ushort" | "c_uint" | "c_ulong"
        | "c_ulonglong" => Some(IntegerKind::FixedUnsigned),
        "i8" | "i16" | "i32" | "i64" | "i128" | "c_schar" | "c_short" | "c_int" | "c_long"
        | "c_longlong" => Some(IntegerKind::FixedSigned),
        "usize" | "isize" | "c_char" => Some(IntegerKind::Other),
        _ => None,
    }
}

/// The block of `{ unsafe { ... } }`, which is how bindgen writes the body of
/// every bitfield getter and setter.
fn sole_unsafe_block(block: &Block) -> Option<&Block> {
    match &block.stmts[..] {
        [Stmt::Expr(Expr::Unsafe(unsafe_block), None)] => Some(&unsafe_block.block),
        _ => None,
    }
}

/// The single argument of a call to `mem::transmute`, however the path to it
/// is spelled.
fn transmute_argument(expr: &Expr) -> Option<&Expr> {
    let Expr::Call(call) = expr else {
        return None;
    };
    let Expr::Path(path) = &*call.func else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let mut segments = path.path.segments.iter().rev();
    if segments.next()?.ident != "transmute" || segments.next()?.ident != "mem" {
        return None;
    }
    match call.args.iter().collect::<Vec<_>>()[..] {
        [arg] => Some(arg),
        _ => None,
    }
}

/// The declared type of the parameter `expr` names, if it names one.
fn parameter_type<'a>(sig: &'a Signature, expr: &Expr) -> Option<&'a Type> {
    let Expr::Path(path) = expr else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let name = path.path.get_ident()?;
    sig.inputs.iter().find_map(|arg| match arg {
        FnArg::Typed(typed) => match &*typed.pat {
            Pat::Ident(ident) if ident.ident == *name => Some(&*typed.ty),
            _ => None,
        },
        FnArg::Receiver(_) => None,
    })
}

/// Whether an accessor which has lost its transmute still needs the `unsafe`
/// block that transmute was in.
#[derive(PartialEq, Eq, Clone, Copy)]
enum StillUnsafe {
    Yes,
    No,
}

/// How a getter or setter reaches the bitfield allocation unit stored in the
/// struct, and whether reaching it that way needs `unsafe`. `None` for
/// anything else, which stops the accessor being rewritten at all: knowing
/// what is left in the body is the whole basis for deciding what to do with
/// the `unsafe` block around it.
///
/// `method` is `get` for a getter and `set` for a setter; the two shapes are
/// otherwise identical.
fn allocation_unit_access(expr: &Expr, method: &str) -> Option<StillUnsafe> {
    let Expr::MethodCall(call) = expr else {
        return None;
    };
    if call.method != method {
        return None;
    }
    match &*call.receiver {
        // `self._bitfield_1.get(..)`, for a bitfield in a struct.
        // `__BindgenBitfieldUnit::get` and `set` are safe.
        Expr::Field(field) if is_self(&field.base) => Some(StillUnsafe::No),
        // `self._bitfield_1.as_ref().get(..)`, for a bitfield in a union
        // bindgen could not make a Rust `union`. `__BindgenUnionField::as_ref`
        // and `as_mut` are `unsafe fn`s, so the block has to stay.
        Expr::MethodCall(unwrap)
            if matches!(unwrap.method.to_string().as_str(), "as_ref" | "as_mut")
                && unwrap.args.is_empty()
                && matches!(&*unwrap.receiver, Expr::Field(field) if is_self(&field.base)) =>
        {
            Some(StillUnsafe::Yes)
        }
        _ => None,
    }
}

/// Whether `expr` is the bare name `name` and nothing more.
fn is_path_ident(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Path(path)
        if path.qself.is_none() && path.path.is_ident(name))
}

fn is_self(expr: &Expr) -> bool {
    is_path_ident(expr, "self")
}

/// An accessor body holding `stmts`, inside an `unsafe` block or not.
fn accessor_body(needs_unsafe: StillUnsafe, stmts: Vec<Stmt>) -> Block {
    match needs_unsafe {
        StillUnsafe::Yes => parse_quote! { { unsafe { #(#stmts)* } } },
        StillUnsafe::No => parse_quote! { { #(#stmts)* } },
    }
}

/// Collapse type-namespace items which share a name within the same
/// bindgen module into a single opaque placeholder, so that the mod
/// compiles instead of hitting E0428.
///
/// `names_duplicated_by_bindgen` is the set gathered by the parse
/// phase. A name outside it is left exactly as bindgen wrote it, even
/// if it does collide, because we have no evidence that both
/// definitions are unusable - see the module documentation.
///
/// Only `struct`/`enum`/`union`/`type` are considered: those are the
/// items bindgen derives from C++ types, and a collision among them
/// is the one we have seen in the wild. Two other collisions are
/// possible in principle and are deliberately left to fail loudly
/// rather than be papered over: one in the value namespace (two
/// `const`s, say), and one between a type and a namespace module,
/// where collapsing would mean deleting the module and everything
/// inside it.
///
/// `impl` blocks for a collapsed name go too. autocxx never uses
/// bindgen's inherent impls (it binds the `extern "C"` declarations
/// instead), and keeping impls from two different C++ types on one
/// placeholder risks a fresh duplicate-method error.
pub(super) fn collapse_colliding_type_names(
    bindgen_mod: &mut ItemMod,
    names_duplicated_by_bindgen: &HashSet<QualifiedName>,
) {
    if names_duplicated_by_bindgen.is_empty() {
        return;
    }
    let Some((_, items)) = &mut bindgen_mod.content else {
        return;
    };
    for item in items {
        // With namespaces enabled bindgen puts everything in a mod
        // called `root`, which is the C++ global namespace; the items
        // beside it are autocxx's own and can never collide.
        if let Item::Mod(root_mod) = item {
            if root_mod.ident == "root" {
                collapse_in_mod(root_mod, &Namespace::new(), names_duplicated_by_bindgen);
            }
        }
    }
}

/// Put the traits `derive!` asked for onto the bindgen definitions of the
/// types which asked.
///
/// It has to be the bindgen definition: a POD struct or an enum is what
/// `ffi::Thing` resolves to, autocxx re-exporting it with a `use` rather than
/// writing a type of its own. (For everything else autocxx does write its own
/// type - an opaque one with no fields - which is why `derive!` refuses to
/// name one of those rather than decorating a definition nobody can reach.)
///
/// Runs after the passes above, so that a `Default` we are asked for is not
/// then stripped back out by [`remove_unwanted_defaults`], and so that we
/// never decorate a placeholder [`collapse_colliding_type_names`] left behind.
pub(super) fn add_requested_derives(bindgen_mod: &mut ItemMod, requested: &DeriveRequests) {
    if requested.is_empty() {
        return;
    }
    let Some((_, items)) = &mut bindgen_mod.content else {
        return;
    };
    for item in items {
        // With namespaces enabled bindgen puts everything in a mod called
        // `root`, which is the C++ global namespace; the items beside it are
        // autocxx's own and are never named by a directive.
        if let Item::Mod(root_mod) = item {
            if root_mod.ident == "root" {
                derive_in_mod(root_mod, &Namespace::new(), requested);
            }
        }
    }
}

fn derive_in_mod(item_mod: &mut ItemMod, ns: &Namespace, requested: &DeriveRequests) {
    let Some((_, items)) = &mut item_mod.content else {
        return;
    };
    for item in items {
        if let Item::Mod(m) = item {
            let child_ns = ns.push(m.ident.to_string());
            derive_in_mod(m, &child_ns, requested);
            continue;
        }
        let Some(ident) = type_namespace_item_ident(item) else {
            continue;
        };
        let Some(traits) = requested.get(&QualifiedName::new(ns, make_ident(ident.to_string())))
        else {
            continue;
        };
        let attrs = match item {
            Item::Struct(s) => &mut s.attrs,
            Item::Enum(e) => &mut e.attrs,
            Item::Union(u) => &mut u.attrs,
            // A type alias cannot derive anything, and `derive!` only ever
            // resolves to a struct or an enum, so this is unreachable in
            // practice.
            _ => continue,
        };
        add_derives_to_attrs(attrs, traits);
    }
}

/// Add each of `wanted` to `attrs`, skipping any which is derived already.
///
/// The skipping matters: bindgen writes `Clone, Hash, PartialEq, Eq` onto
/// every enum of its own accord, and a second `#[derive(Clone)]` is a
/// conflicting implementation rather than a no-op. Traits are compared by
/// their final path segment, which is how they are spelled in the lists
/// bindgen writes; two different traits of the same name would be taken for
/// one, and erring that way costs a derive rather than a compile error.
fn add_derives_to_attrs(attrs: &mut Vec<Attribute>, wanted: &[Path]) {
    let mut already: HashSet<String> = HashSet::new();
    for attr in attrs.iter() {
        if !attr.path().is_ident("derive") {
            continue;
        }
        // A derive list we cannot read is one we cannot check against, so
        // leave it be and add ours regardless; a duplicate is a clearer
        // complaint than a silently missing trait.
        let _ = attr.parse_nested_meta(|meta| {
            if let Some(name) = final_segment(&meta.path) {
                already.insert(name);
            }
            Ok(())
        });
    }
    let to_add: Vec<&Path> = wanted
        .iter()
        .filter(|path| !final_segment(path).is_some_and(|name| already.contains(&name)))
        .collect();
    if to_add.is_empty() {
        return;
    }
    attrs.push(parse_quote! { #[derive(#(#to_add),*)] });
}

fn final_segment(path: &Path) -> Option<String> {
    path.segments.last().map(|seg| seg.ident.to_string())
}

fn collapse_in_mod(
    item_mod: &mut ItemMod,
    ns: &Namespace,
    names_duplicated_by_bindgen: &HashSet<QualifiedName>,
) {
    let Some((_, items)) = &mut item_mod.content else {
        return;
    };
    // A name is only worth collapsing if it really is defined more than
    // once here. The parse phase can record a duplicate whose emitted
    // definitions are not both type items, and rewriting a lone
    // definition would change its meaning for no gain.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in items.iter() {
        if let Some(name) = type_namespace_item_name(item) {
            *counts.entry(name).or_default() += 1;
        }
    }
    let to_collapse: HashSet<String> = counts
        .into_iter()
        .filter(|(name, count)| {
            *count > 1
                && names_duplicated_by_bindgen.contains(&QualifiedName::new(ns, make_ident(name)))
        })
        .map(|(name, _)| name)
        .collect();
    if !to_collapse.is_empty() {
        let mut collapsed: HashSet<String> = HashSet::new();
        let mut replaced = Vec::with_capacity(items.len());
        for item in items.drain(..) {
            if let Some(ident) =
                type_namespace_item_ident(&item).filter(|id| to_collapse.contains(&id.to_string()))
            {
                // Emit the placeholder where the first of the colliding
                // definitions stood, so the surrounding items keep their
                // order, and drop the rest.
                if collapsed.insert(ident.to_string()) {
                    log::info!(
                        "Multiple bindgen items are named {ident}; replacing them all with an opaque type."
                    );
                    replaced.push(parse_quote! {
                        #[repr(C)]
                        pub struct #ident {
                            _unused: [u8; 0],
                        }
                    });
                }
            } else if impl_self_type_name(&item).is_some_and(|name| to_collapse.contains(&name)) {
                // Drop the impl block along with the type it was for.
            } else {
                replaced.push(item);
            }
        }
        *items = replaced;
    }
    for item in items {
        if let Item::Mod(m) = item {
            let child_ns = ns.push(m.ident.to_string());
            collapse_in_mod(m, &child_ns, names_duplicated_by_bindgen);
        }
    }
}

/// The identifier an item binds in the type namespace, for the kinds of
/// item bindgen generates from C++ types.
fn type_namespace_item_ident(item: &Item) -> Option<&Ident> {
    match item {
        Item::Struct(s) => Some(&s.ident),
        Item::Enum(e) => Some(&e.ident),
        Item::Union(u) => Some(&u.ident),
        Item::Type(t) => Some(&t.ident),
        _ => None,
    }
}

fn type_namespace_item_name(item: &Item) -> Option<String> {
    type_namespace_item_ident(item).map(Ident::to_string)
}

/// The bare name of the type an inherent `impl` block is for, if it is
/// written as a single path segment (which is how bindgen writes them).
fn impl_self_type_name(item: &Item) -> Option<String> {
    match item {
        Item::Impl(i) if i.trait_.is_none() => match &*i.self_ty {
            Type::Path(tp) if tp.qself.is_none() && tp.path.segments.len() == 1 => {
                Some(tp.path.segments[0].ident.to_string())
            }
            _ => None,
        },
        _ => None,
    }
}

fn collect_defined_type_names(item_mod: &ItemMod, defined: &mut HashSet<String>) {
    if let Some((_, items)) = &item_mod.content {
        for item in items {
            if let Some(name) = type_namespace_item_name(item) {
                defined.insert(name);
                continue;
            }
            match item {
                // Imports bind bare names too. In particular autocxx
                // injects `use super::{...}` and `use autocxx::c_char16_t
                // as bindgen_cchar16_t` into every bindgen module, and
                // aliases like `pub type Foo = bindgen_cchar16_t;` are
                // legitimate.
                Item::Use(u) => collect_use_names(&u.tree, defined),
                Item::Mod(m) => collect_defined_type_names(m, defined),
                _ => {}
            }
        }
    }
}

fn collect_use_names(tree: &UseTree, defined: &mut HashSet<String>) {
    match tree {
        UseTree::Path(p) => collect_use_names(&p.tree, defined),
        UseTree::Name(n) => {
            defined.insert(n.ident.to_string());
        }
        UseTree::Rename(r) => {
            defined.insert(r.rename.to_string());
        }
        UseTree::Group(g) => {
            for t in &g.items {
                collect_use_names(t, defined);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn prune_mod(item_mod: &mut ItemMod, defined: &HashSet<String>, pruned: &mut Vec<String>) {
    if let Some((_, items)) = &mut item_mod.content {
        items.retain(|item| match item {
            Item::Type(t) => {
                let params: HashSet<String> = t
                    .generics
                    .params
                    .iter()
                    .filter_map(|p| match p {
                        GenericParam::Type(tp) => Some(tp.ident.to_string()),
                        _ => None,
                    })
                    .collect();
                if has_unbound_ident(&t.ty, &params, defined) {
                    pruned.push(t.ident.to_string());
                    false
                } else {
                    true
                }
            }
            _ => true,
        });
        for item in items {
            if let Item::Mod(m) = item {
                prune_mod(m, defined, pruned);
            }
        }
    }
}

fn is_primitive(ident: &str) -> bool {
    matches!(
        ident,
        "bool"
            | "char"
            | "str"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
    )
}

fn has_unbound_ident(ty: &Type, params: &HashSet<String>, defined: &HashSet<String>) -> bool {
    match ty {
        Type::Path(tp) => {
            if let Some(qself) = &tp.qself {
                if has_unbound_ident(&qself.ty, params, defined) {
                    return true;
                }
            }
            if tp.qself.is_none()
                && tp.path.leading_colon.is_none()
                && tp.path.segments.len() == 1
                && tp.path.segments[0].arguments.is_none()
            {
                let ident = tp.path.segments[0].ident.to_string();
                if !params.contains(&ident) && !defined.contains(&ident) && !is_primitive(&ident) {
                    return true;
                }
            }
            tp.path.segments.iter().any(|seg| match &seg.arguments {
                PathArguments::AngleBracketed(ab) => ab.args.iter().any(|arg| match arg {
                    GenericArgument::Type(ty) => has_unbound_ident(ty, params, defined),
                    GenericArgument::AssocType(at) => has_unbound_ident(&at.ty, params, defined),
                    _ => false,
                }),
                PathArguments::Parenthesized(p) => {
                    p.inputs
                        .iter()
                        .any(|ty| has_unbound_ident(ty, params, defined))
                        || match &p.output {
                            ReturnType::Type(_, ty) => has_unbound_ident(ty, params, defined),
                            ReturnType::Default => false,
                        }
                }
                PathArguments::None => false,
            })
        }
        Type::Reference(r) => has_unbound_ident(&r.elem, params, defined),
        Type::Ptr(p) => has_unbound_ident(&p.elem, params, defined),
        Type::Slice(s) => has_unbound_ident(&s.elem, params, defined),
        Type::Array(a) => has_unbound_ident(&a.elem, params, defined),
        Type::Group(g) => has_unbound_ident(&g.elem, params, defined),
        Type::Paren(p) => has_unbound_ident(&p.elem, params, defined),
        Type::Tuple(t) => t
            .elems
            .iter()
            .any(|ty| has_unbound_ident(ty, params, defined)),
        // For trait objects and impl-trait, only inspect the generic
        // arguments of the bounds; the trait names themselves are not
        // collected in `defined`, so checking them would false-positive.
        Type::TraitObject(t) => t
            .bounds
            .iter()
            .any(|b| bound_has_unbound_ident(b, params, defined)),
        Type::ImplTrait(t) => t
            .bounds
            .iter()
            .any(|b| bound_has_unbound_ident(b, params, defined)),
        Type::BareFn(f) => {
            f.inputs
                .iter()
                .any(|arg| has_unbound_ident(&arg.ty, params, defined))
                || match &f.output {
                    ReturnType::Type(_, ty) => has_unbound_ident(ty, params, defined),
                    ReturnType::Default => false,
                }
        }
        // Type::Macro, Type::Verbatim etc.: we can't see inside, so
        // conservatively keep the alias (false negatives are safe;
        // false positives would remove legitimate API).
        _ => false,
    }
}

fn bound_has_unbound_ident(
    bound: &TypeParamBound,
    params: &HashSet<String>,
    defined: &HashSet<String>,
) -> bool {
    match bound {
        TypeParamBound::Trait(tb) => tb.path.segments.iter().any(|seg| match &seg.arguments {
            PathArguments::AngleBracketed(ab) => ab.args.iter().any(|arg| match arg {
                GenericArgument::Type(ty) => has_unbound_ident(ty, params, defined),
                GenericArgument::AssocType(at) => has_unbound_ident(&at.ty, params, defined),
                _ => false,
            }),
            PathArguments::Parenthesized(p) => {
                p.inputs
                    .iter()
                    .any(|ty| has_unbound_ident(ty, params, defined))
                    || match &p.output {
                        ReturnType::Type(_, ty) => has_unbound_ident(ty, params, defined),
                        ReturnType::Default => false,
                    }
            }
            PathArguments::None => false,
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn count_aliases(item_mod: &ItemMod) -> usize {
        let mut count = 0;
        fn walk(item_mod: &ItemMod, count: &mut usize) {
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    match item {
                        Item::Type(_) => *count += 1,
                        Item::Mod(m) => walk(m, count),
                        _ => {}
                    }
                }
            }
        }
        walk(item_mod, &mut count);
        count
    }

    #[test]
    fn removes_alias_with_unbound_param() {
        // The google/autocxx#1480 shape: RHS names _CharT but the
        // alias declares no generics and _CharT is defined nowhere.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct basic_string_view {
                    _p: u8,
                }
                pub type basic_string___self_view = root::std::basic_string_view<_CharT>;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 0);
    }

    #[test]
    fn keeps_alias_with_declared_param() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct basic_stream {
                    _p: u8,
                }
                pub type sentry_stream_type<_CharT> = root::basic_stream<_CharT>;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }

    #[test]
    fn keeps_alias_to_defined_and_primitive_types() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct Concrete {
                    _p: u8,
                }
                pub type A = Concrete;
                pub type B = u32;
                pub type C = *mut Concrete;
                pub type D = ::std::os::raw::c_char;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 4);
    }

    #[test]
    fn keeps_alias_to_imported_name() {
        // autocxx injects imports (including a rename) into every
        // bindgen module; aliases to those names are legitimate.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                #[allow(unused_imports)]
                use super::{cxxbridge, output};
                use autocxx::c_char16_t as bindgen_cchar16_t;
                pub type Foo = bindgen_cchar16_t;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }

    #[test]
    fn prunes_alias_chains_to_fixpoint() {
        // GoodLooking references BadAlias which itself gets pruned;
        // both must go.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct Real {
                    _p: u8,
                }
                pub type BadAlias = Real<_CharT>;
                pub type GoodLooking = BadAlias;
                pub type Unaffected = Real;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }

    #[test]
    fn removes_unbound_in_trait_object_bound_args() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub type Bad = *const dyn SomeTrait<_CharT>;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 0);
    }

    #[test]
    fn removes_unbound_in_nested_mod_and_nested_position() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub mod std {
                        pub type bad = super::basic_thing<_Traits>;
                        pub type bad_ref = *const _Pointer;
                        pub type good = u8;
                    }
                }
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }

    /// The names bound in the type namespace by the items directly
    /// inside the named module, in order.
    fn type_names_in(item_mod: &ItemMod, wanted: &str) -> Vec<String> {
        fn walk(item_mod: &ItemMod, wanted: &str, out: &mut Vec<String>) {
            if item_mod.ident == wanted {
                if let Some((_, items)) = &item_mod.content {
                    out.extend(items.iter().filter_map(type_namespace_item_name));
                }
            }
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    if let Item::Mod(m) = item {
                        walk(m, wanted, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(item_mod, wanted, &mut out);
        out
    }

    /// The set the parse phase hands us, written in C++ form: `iterator`
    /// lives in the global namespace, `a::iterator` in namespace `a`.
    fn duplicated_names(names: &[&str]) -> HashSet<QualifiedName> {
        names
            .iter()
            .copied()
            .map(QualifiedName::new_from_cpp_name)
            .collect()
    }

    #[test]
    fn collapses_colliding_type_names() {
        // The google/autocxx#490 shape: members of two different class
        // template specializations both land on `root::iterator`.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type iterator = root::pointer;
                    pub type pointer = *mut root::Elem;
                    #[repr(C)]
                    pub struct Cursor {
                        pub it: root::iterator,
                    }
                    #[repr(C)]
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                }
            }
        };
        collapse_colliding_type_names(&mut m, &duplicated_names(&["iterator"]));
        assert_eq!(
            type_names_in(&m, "root"),
            vec!["iterator", "pointer", "Cursor"]
        );
    }

    #[test]
    fn collapses_more_than_two_colliding_names() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type iterator = u8;
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                    pub union iterator {
                        a: u8,
                    }
                    pub enum iterator {
                        A,
                    }
                }
            }
        };
        collapse_colliding_type_names(&mut m, &duplicated_names(&["iterator"]));
        assert_eq!(type_names_in(&m, "root"), vec!["iterator"]);
    }

    #[test]
    fn collapsing_drops_impls_of_the_collapsed_type() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub struct iterator {
                        pub a: u8,
                    }
                    impl iterator {
                        pub fn get(&self) -> u8 {
                            self.a
                        }
                    }
                    pub type iterator = u8;
                    pub struct other {
                        _unused: [u8; 0],
                    }
                    impl other {
                        pub fn ok() {}
                    }
                }
            }
        };
        collapse_colliding_type_names(&mut m, &duplicated_names(&["iterator"]));
        let items = match &m.content.as_ref().unwrap().1[0] {
            Item::Mod(root) => root.content.as_ref().unwrap().1.clone(),
            _ => panic!("expected root mod"),
        };
        assert_eq!(
            items
                .iter()
                .filter_map(impl_self_type_name)
                .collect::<Vec<_>>(),
            vec!["other"]
        );
    }

    /// Assert that collapsing left the mod exactly as bindgen wrote it.
    fn assert_collapse_is_a_no_op(m: &ItemMod, duplicated: &HashSet<QualifiedName>) {
        let mut after = m.clone();
        collapse_colliding_type_names(&mut after, duplicated);
        assert_eq!(
            quote::ToTokens::to_token_stream(&after).to_string(),
            quote::ToTokens::to_token_stream(m).to_string()
        );
    }

    #[test]
    fn collapsing_leaves_distinct_and_differently_scoped_names_alone() {
        // Same name in two different modules is not a collision, and
        // neither is a type sharing a name with a function - however the
        // parse phase came to record those names as duplicated.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub mod a {
                        pub struct iterator {
                            _unused: [u8; 0],
                        }
                    }
                    pub mod b {
                        pub struct iterator {
                            _unused: [u8; 0],
                        }
                    }
                    pub struct thing {
                        _unused: [u8; 0],
                    }
                    pub fn thing() {}
                }
            }
        };
        assert_collapse_is_a_no_op(
            &m,
            &duplicated_names(&["a::iterator", "b::iterator", "thing"]),
        );
    }

    #[test]
    fn collapsing_leaves_a_collision_the_parse_phase_did_not_record_alone() {
        // Two `iterator`s in one mod, but the parse phase did not record
        // that name as duplicated: it skipped one of the declarations for
        // its own reasons, and the alias survived as a real type which
        // other items may legitimately refer to. Collapsing on the
        // syntactic collision alone would turn that alias into a
        // zero-sized struct and quietly change the FFI signatures using
        // it, so the mod is emitted unaltered. The set is non-empty so
        // that the per-name gate is what the test exercises, not the
        // empty-set shortcut.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type iterator = *mut root::Elem;
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                    #[repr(C)]
                    pub struct Cursor {
                        pub it: root::iterator,
                    }
                }
            }
        };
        assert_collapse_is_a_no_op(&m, &duplicated_names(&["Elem"]));
    }

    #[test]
    fn collapsing_leaves_a_type_colliding_with_a_module_alone() {
        // Collapsing would mean deleting the module and everything in
        // it, so this collision is left to fail loudly instead.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                    pub mod iterator {
                        pub struct Inner {
                            _unused: [u8; 0],
                        }
                    }
                }
            }
        };
        assert_collapse_is_a_no_op(&m, &duplicated_names(&["iterator"]));
    }

    #[test]
    fn collapsing_leaves_a_value_namespace_collision_alone() {
        // An opaque struct is no substitute for a constant, so a
        // collision in the value namespace is left to fail loudly too.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub const LIMIT: u32 = 1;
                    pub const LIMIT: u32 = 2;
                }
            }
        };
        assert_collapse_is_a_no_op(&m, &duplicated_names(&["LIMIT"]));
    }

    /// The bindgen output a `derive_default` build produces, in miniature:
    /// a struct which could derive `Default`, one which could not and so got
    /// a zero-filling impl of bindgen's own, and the two helper impls bindgen
    /// writes whatever we asked for.
    fn mod_with_default_impls() -> ItemMod {
        parse_quote! {
            mod bindgen {
                pub mod root {
                    #[repr(C)]
                    #[derive(Default)]
                    pub struct Bitfieldy {
                        pub _bitfield_1: root::__BindgenBitfieldUnit<[u8; 1usize]>,
                    }
                    #[repr(u32)]
                    #[derive(Default, Clone, Hash, PartialEq, Eq)]
                    pub enum Fruit {
                        APPLE = 1,
                        PEAR = 2,
                    }
                    #[repr(u32)]
                    #[derive(Default)]
                    pub enum Lonely {
                        ONLY = 1,
                    }
                    #[repr(C)]
                    pub struct Basket {
                        pub fruit: root::Fruit,
                    }
                    impl Default for Basket {
                        fn default() -> Self {
                            let mut s = ::core::mem::MaybeUninit::<Self>::uninit();
                            unsafe {
                                ::core::ptr::write_bytes(s.as_mut_ptr(), 0, 1);
                                s.assume_init()
                            }
                        }
                    }
                    impl<T> ::core::default::Default for __BindgenUnionField<T> {
                        #[inline]
                        fn default() -> Self {
                            Self::new()
                        }
                    }
                    impl<T: Copy + Default, const N: usize> Default for __BindgenOpaqueArray<T, N> {
                        fn default() -> Self {
                            Self([<T as Default>::default(); N])
                        }
                    }
                }
            }
        }
    }

    fn default_impl_self_types(item_mod: &ItemMod) -> Vec<String> {
        let mut found = Vec::new();
        fn walk(item_mod: &ItemMod, found: &mut Vec<String>) {
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    match item {
                        Item::Impl(i) if i.trait_.is_some() => {
                            found.push(i.self_ty.to_token_stream().to_string())
                        }
                        Item::Mod(m) => walk(m, found),
                        _ => {}
                    }
                }
            }
        }
        walk(item_mod, &mut found);
        found
    }

    #[test]
    fn strips_only_the_zero_filling_default_impl() {
        let mut m = mod_with_default_impls();
        remove_unwanted_defaults(&mut m);
        let remaining = default_impl_self_types(&m);
        // The zero-filler for a type holding an enum is the unsound one.
        assert!(
            !remaining.iter().any(|ty| ty == "Basket"),
            "zero-filling impl survived: {remaining:?}"
        );
        // bindgen's own helpers construct a value properly; they stay.
        assert!(
            remaining
                .iter()
                .any(|ty| ty.contains("__BindgenUnionField")),
            "__BindgenUnionField impl was removed: {remaining:?}"
        );
        assert!(
            remaining
                .iter()
                .any(|ty| ty.contains("__BindgenOpaqueArray")),
            "__BindgenOpaqueArray impl was removed: {remaining:?}"
        );
    }

    #[test]
    fn leaves_a_structs_derived_default_alone() {
        let mut m = mod_with_default_impls();
        remove_unwanted_defaults(&mut m);
        // A struct's derive delegates to its fields, and is the whole point
        // of asking bindgen for `derive_default` in the first place.
        let attrs = derives_of(&m, "Bitfieldy");
        assert!(
            attrs.contains(&"Default".to_string()),
            "the struct's derive was disturbed: {attrs:?}"
        );
    }

    /// Nothing makes one enumerator the default, and rustc rejects the derive
    /// outright (E0665), so `Default` comes off every enum - but only
    /// `Default`.
    #[test]
    fn strips_default_from_enum_derives_and_keeps_the_rest() {
        let mut m = mod_with_default_impls();
        remove_unwanted_defaults(&mut m);
        let attrs = derives_of(&m, "Fruit");
        assert!(
            !attrs.contains(&"Default".to_string()),
            "Default survived on an enum: {attrs:?}"
        );
        assert_eq!(
            attrs,
            vec!["Clone", "Hash", "PartialEq", "Eq"],
            "the other derives were disturbed"
        );
    }

    #[test]
    fn drops_the_derive_attribute_when_only_default_was_in_it() {
        let mut m = mod_with_default_impls();
        remove_unwanted_defaults(&mut m);
        // An empty `#[derive()]` is legal but pointless; check we removed the
        // attribute rather than emptying it.
        assert!(
            derives_of(&m, "Lonely").is_empty(),
            "expected no derive attribute at all"
        );
        let rendered = enum_tokens(&m, "Lonely");
        assert!(
            !rendered.contains("derive"),
            "an empty derive was left behind: {rendered}"
        );
    }

    /// The derive list of the named enum or struct, as plain strings.
    /// A mod shaped the way bindgen writes namespaced types, for the
    /// `derive!` pass to work over.
    fn mod_with_namespaced_types() -> ItemMod {
        parse_quote! {
            mod bindgen {
                pub mod root {
                    #[repr(C)]
                    #[derive(Default)]
                    pub struct Point {
                        pub x: u32,
                    }
                    #[repr(u32)]
                    #[derive(Clone, Hash, PartialEq, Eq)]
                    pub enum Fruit {
                        APPLE = 1,
                    }
                    pub mod ns {
                        #[repr(C)]
                        pub struct Point {
                            pub y: u32,
                        }
                    }
                }
            }
        }
    }

    fn derive_requests(entries: &[(&str, &[&str])]) -> DeriveRequests {
        entries
            .iter()
            .map(|(name, traits)| {
                (
                    QualifiedName::new_from_cpp_name(name),
                    traits
                        .iter()
                        .map(|t| syn::parse_str::<Path>(t).unwrap())
                        .collect(),
                )
            })
            .collect()
    }

    /// The derives on the type at `path`, which for these tests has to name
    /// the enclosing mods too: the fixture deliberately has two types called
    /// `Point`, and telling them apart is the point. Unlike [`derives_of`],
    /// this looks only in the mod the path names, never inside its children.
    fn derives_at(item_mod: &ItemMod, path: &[&str]) -> Vec<String> {
        let (name, mods) = path.split_last().expect("a path names something");
        let mut here = item_mod;
        for mod_name in mods {
            let (_, items) = here.content.as_ref().expect("an inline mod");
            here = items
                .iter()
                .find_map(|item| match item {
                    Item::Mod(m) if m.ident == mod_name => Some(m),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no mod {mod_name}"));
        }
        let (_, items) = here.content.as_ref().expect("an inline mod");
        let attrs = items
            .iter()
            .find_map(|item| match item {
                Item::Enum(e) if e.ident == name => Some(&e.attrs),
                Item::Struct(s) if s.ident == name => Some(&s.attrs),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no type {name}"));
        let mut found = Vec::new();
        for attr in attrs {
            if !attr.path().is_ident("derive") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                found.push(meta.path.to_token_stream().to_string());
                Ok(())
            })
            .unwrap();
        }
        found
    }

    #[test]
    fn adds_requested_derives_to_the_named_type_only() {
        let mut m = mod_with_namespaced_types();
        add_requested_derives(
            &mut m,
            &derive_requests(&[("Point", &["Debug", "PartialEq"])]),
        );
        assert_eq!(
            derives_at(&m, &["root", "Point"]),
            vec!["Default", "Debug", "PartialEq"]
        );
        assert!(derives_at(&m, &["root", "ns", "Point"]).is_empty());
    }

    #[test]
    fn tells_two_namespaces_apart() {
        let mut m = mod_with_namespaced_types();
        add_requested_derives(&mut m, &derive_requests(&[("ns::Point", &["Debug"])]));
        assert_eq!(derives_at(&m, &["root", "ns", "Point"]), vec!["Debug"]);
        assert_eq!(derives_at(&m, &["root", "Point"]), vec!["Default"]);
    }

    /// bindgen writes `Clone, Hash, PartialEq, Eq` onto every enum itself, and
    /// a second `#[derive(Clone)]` is a conflicting implementation.
    #[test]
    fn does_not_repeat_a_derive_bindgen_already_wrote() {
        let mut m = mod_with_namespaced_types();
        add_requested_derives(&mut m, &derive_requests(&[("Fruit", &["Clone", "Debug"])]));
        assert_eq!(
            derives_of(&m, "Fruit"),
            vec!["Clone", "Hash", "PartialEq", "Eq", "Debug"]
        );
    }

    #[test]
    fn leaves_unmentioned_types_alone() {
        let mut m = mod_with_namespaced_types();
        add_requested_derives(&mut m, &derive_requests(&[("Point", &["Debug"])]));
        assert_eq!(
            derives_of(&m, "Fruit"),
            vec!["Clone", "Hash", "PartialEq", "Eq"]
        );
    }

    fn derives_of(item_mod: &ItemMod, name: &str) -> Vec<String> {
        let mut found = Vec::new();
        for attr in attrs_of(item_mod, name) {
            if !attr.path().is_ident("derive") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                found.push(meta.path.to_token_stream().to_string());
                Ok(())
            })
            .unwrap();
        }
        found
    }

    fn attrs_of(item_mod: &ItemMod, name: &str) -> Vec<Attribute> {
        let mut found = Vec::new();
        fn walk(item_mod: &ItemMod, name: &str, found: &mut Vec<Attribute>) {
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    match item {
                        Item::Enum(e) if e.ident == name => found.extend(e.attrs.iter().cloned()),
                        Item::Struct(s) if s.ident == name => found.extend(s.attrs.iter().cloned()),
                        Item::Mod(m) => walk(m, name, found),
                        _ => {}
                    }
                }
            }
        }
        walk(item_mod, name, &mut found);
        found
    }

    fn enum_tokens(item_mod: &ItemMod, name: &str) -> String {
        fn walk(item_mod: &ItemMod, name: &str, out: &mut String) {
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    match item {
                        Item::Enum(e) if e.ident == name => *out = e.to_token_stream().to_string(),
                        Item::Mod(m) => walk(m, name, out),
                        _ => {}
                    }
                }
            }
        }
        let mut out = String::new();
        walk(item_mod, name, &mut out);
        out
    }

    /// The named method of the first `impl` block in the mod, as tokens with
    /// the whitespace normalized away, so that a test can say what it expects
    /// without minding how `quote` spaces it.
    fn method_tokens(item_mod: &ItemMod, name: &str) -> String {
        let mut found = None;
        fn walk(item_mod: &ItemMod, name: &str, found: &mut Option<String>) {
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    match item {
                        Item::Impl(imp) => {
                            for impl_item in &imp.items {
                                if let ImplItem::Fn(f) = impl_item {
                                    if f.sig.ident == name {
                                        *found = Some(f.to_token_stream().to_string());
                                    }
                                }
                            }
                        }
                        Item::Mod(m) => walk(m, name, found),
                        _ => {}
                    }
                }
            }
        }
        walk(item_mod, name, &mut found);
        found.unwrap_or_else(|| panic!("no method called {name}"))
    }

    fn tokens_of(item: impl ToTokens) -> String {
        item.to_token_stream().to_string()
    }

    /// The accessors bindgen writes for a struct of bitfields, one field per
    /// interesting field type: an unsigned C type spelled as an alias of the
    /// allocation unit's own integer, a signed one, `bool`, and an `enum`
    /// which no cast can produce.
    fn mod_with_bitfield_accessors() -> ItemMod {
        parse_quote! {
            mod bindgen {
                pub mod root {
                    impl Lots {
                        #[inline]
                        pub fn plain_unsigned(&self) -> ::std::os::raw::c_uint {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(0usize, 4u8) as u32)
                            }
                        }
                        #[inline]
                        pub fn set_plain_unsigned(&mut self, val: ::std::os::raw::c_uint) {
                            unsafe {
                                let val: u32 = ::std::mem::transmute(val);
                                self._bitfield_1.set(0usize, 4u8, val as u64)
                            }
                        }
                        #[inline]
                        pub fn plain_int(&self) -> ::std::os::raw::c_int {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(4usize, 4u8) as u32)
                            }
                        }
                        #[inline]
                        pub fn set_plain_int(&mut self, val: ::std::os::raw::c_int) {
                            unsafe {
                                let val: u32 = ::std::mem::transmute(val);
                                self._bitfield_1.set(4usize, 4u8, val as u64)
                            }
                        }
                        #[inline]
                        pub fn flag(&self) -> bool {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(8usize, 1u8) as u8)
                            }
                        }
                        #[inline]
                        pub fn set_flag(&mut self, val: bool) {
                            unsafe {
                                let val: u8 = ::std::mem::transmute(val);
                                self._bitfield_1.set(8usize, 1u8, val as u64)
                            }
                        }
                        #[inline]
                        pub unsafe fn flag_raw(this: *const Self) -> bool {
                            unsafe {
                                ::std::mem::transmute(<root::__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                                    ::std::ptr::addr_of!((*this)._bitfield_1),
                                    8usize,
                                    1u8,
                                ) as u8)
                            }
                        }
                        #[inline]
                        pub unsafe fn set_flag_raw(this: *mut Self, val: bool) {
                            unsafe {
                                let val: u8 = ::std::mem::transmute(val);
                                <root::__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                                    ::std::ptr::addr_of_mut!((*this)._bitfield_1),
                                    8usize,
                                    1u8,
                                    val as u64,
                                )
                            }
                        }
                        #[inline]
                        pub fn shade(&self) -> root::Shade {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(9usize, 2u8) as u32)
                            }
                        }
                        #[inline]
                        pub fn set_shade(&mut self, val: root::Shade) {
                            unsafe {
                                let val: u32 = ::std::mem::transmute(val);
                                self._bitfield_1.set(9usize, 2u8, val as u64)
                            }
                        }
                        #[inline]
                        pub fn new_bitfield_1(
                            plain_int: ::std::os::raw::c_int,
                            flag: bool,
                            shade: root::Shade,
                        ) -> root::__BindgenBitfieldUnit<[u8; 2usize]> {
                            let mut __bindgen_bitfield_unit: root::__BindgenBitfieldUnit<[u8; 2usize]> =
                                Default::default();
                            __bindgen_bitfield_unit.set(4usize, 4u8, {
                                let plain_int: u32 = unsafe { ::std::mem::transmute(plain_int) };
                                plain_int as u64
                            });
                            __bindgen_bitfield_unit.set(8usize, 1u8, {
                                let flag: u8 = unsafe { ::std::mem::transmute(flag) };
                                flag as u64
                            });
                            __bindgen_bitfield_unit.set(9usize, 2u8, {
                                let shade: u32 = unsafe { ::std::mem::transmute(shade) };
                                shade as u64
                            });
                            __bindgen_bitfield_unit
                        }
                    }
                }
            }
        }
    }

    /// A signed field's accessors cast, in both directions, and lose the
    /// `unsafe` block they no longer need.
    #[test]
    fn casts_between_integers_of_differing_signedness() {
        let mut m = mod_with_bitfield_accessors();
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "plain_int"),
            expected_method(parse_quote! {
                #[inline]
                pub fn plain_int(&self) -> ::std::os::raw::c_int {
                    self._bitfield_1.get(4usize, 4u8) as u32 as ::std::os::raw::c_int
                }
            })
        );
        assert_eq!(
            method_tokens(&m, "set_plain_int"),
            expected_method(parse_quote! {
                #[inline]
                pub fn set_plain_int(&mut self, val: ::std::os::raw::c_int) {
                    let val: u32 = val as u32;
                    self._bitfield_1.set(4usize, 4u8, val as u64)
                }
            })
        );
    }

    /// `unsigned` and the `u32` its allocation unit deals in are one type
    /// under two names, so neither direction needs a cast at all - writing one
    /// would be `clippy::unnecessary_cast`.
    #[test]
    fn writes_no_cast_between_two_names_for_one_unsigned_type() {
        let mut m = mod_with_bitfield_accessors();
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "plain_unsigned"),
            expected_method(parse_quote! {
                #[inline]
                pub fn plain_unsigned(&self) -> ::std::os::raw::c_uint {
                    self._bitfield_1.get(0usize, 4u8) as u32
                }
            })
        );
        assert_eq!(
            method_tokens(&m, "set_plain_unsigned"),
            expected_method(parse_quote! {
                #[inline]
                pub fn set_plain_unsigned(&mut self, val: ::std::os::raw::c_uint) {
                    let val: u32 = val;
                    self._bitfield_1.set(0usize, 4u8, val as u64)
                }
            })
        );
    }

    /// A `bool` reads back as a comparison and writes as a cast.
    #[test]
    fn compares_against_zero_for_a_bool_field() {
        let mut m = mod_with_bitfield_accessors();
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "flag"),
            expected_method(parse_quote! {
                #[inline]
                pub fn flag(&self) -> bool {
                    self._bitfield_1.get(8usize, 1u8) as u8 != 0
                }
            })
        );
        assert_eq!(
            method_tokens(&m, "set_flag"),
            expected_method(parse_quote! {
                #[inline]
                pub fn set_flag(&mut self, val: bool) {
                    let val: u8 = val as u8;
                    self._bitfield_1.set(8usize, 1u8, val as u64)
                }
            })
        );
    }

    /// The `_raw` accessors dereference a raw pointer, so their `unsafe` block
    /// stays even once the transmute inside it is gone.
    #[test]
    fn keeps_the_unsafe_block_of_a_raw_accessor() {
        let mut m = mod_with_bitfield_accessors();
        simplify_bitfield_transmutes(&mut m);
        for name in ["flag_raw", "set_flag_raw"] {
            let rendered = method_tokens(&m, name);
            assert!(
                !rendered.contains("transmute"),
                "{name} kept its transmute: {rendered}"
            );
            assert!(
                rendered.contains("unsafe {"),
                "{name} lost the unsafe block it still needs: {rendered}"
            );
        }
    }

    /// Each field of the allocation unit constructor is converted on its own,
    /// so the `enum` among them keeps its transmute while its neighbours lose
    /// theirs.
    #[test]
    fn converts_the_bitfield_unit_constructor_field_by_field() {
        let mut m = mod_with_bitfield_accessors();
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "new_bitfield_1"),
            expected_method(parse_quote! {
                #[inline]
                pub fn new_bitfield_1(
                    plain_int: ::std::os::raw::c_int,
                    flag: bool,
                    shade: root::Shade,
                ) -> root::__BindgenBitfieldUnit<[u8; 2usize]> {
                    let mut __bindgen_bitfield_unit: root::__BindgenBitfieldUnit<[u8; 2usize]> =
                        Default::default();
                    __bindgen_bitfield_unit.set(4usize, 4u8, {
                        let plain_int: u32 = plain_int as u32;
                        plain_int as u64
                    });
                    __bindgen_bitfield_unit.set(8usize, 1u8, {
                        let flag: u8 = flag as u8;
                        flag as u64
                    });
                    __bindgen_bitfield_unit.set(9usize, 2u8, {
                        let shade: u32 = unsafe { ::std::mem::transmute(shade) };
                        shade as u64
                    });
                    __bindgen_bitfield_unit
                }
            })
        );
    }

    /// No cast turns an integer into a Rust `enum`, so a bitfield of C++
    /// `enum` type is left exactly as bindgen wrote it - `unsafe` block and
    /// all. rustc agrees: `unnecessary_transmutes` does not fire on it either.
    #[test]
    fn leaves_an_enum_bitfield_alone() {
        let before = mod_with_bitfield_accessors();
        let mut after = before.clone();
        simplify_bitfield_transmutes(&mut after);
        for name in ["shade", "set_shade"] {
            assert_eq!(
                method_tokens(&after, name),
                method_tokens(&before, name),
                "{name} was rewritten"
            );
        }
    }

    /// A bitfield of a `union` reaches its allocation unit through
    /// `__BindgenUnionField::as_ref`, which is an `unsafe fn`: the transmute
    /// goes but the block around it stays, or the accessor stops compiling.
    #[test]
    fn keeps_the_unsafe_block_of_a_union_field_accessor() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    impl Unioned {
                        #[inline]
                        pub fn flag(&self) -> bool {
                            unsafe {
                                ::std::mem::transmute(
                                    self._bitfield_1.as_ref().get(0usize, 1u8) as u8
                                )
                            }
                        }
                        #[inline]
                        pub fn set_flag(&mut self, val: bool) {
                            unsafe {
                                let val: u8 = ::std::mem::transmute(val);
                                self._bitfield_1.as_mut().set(0usize, 1u8, val as u64)
                            }
                        }
                    }
                }
            }
        };
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "flag"),
            expected_method(parse_quote! {
                #[inline]
                pub fn flag(&self) -> bool {
                    unsafe { self._bitfield_1.as_ref().get(0usize, 1u8) as u8 != 0 }
                }
            })
        );
        assert_eq!(
            method_tokens(&m, "set_flag"),
            expected_method(parse_quote! {
                #[inline]
                pub fn set_flag(&mut self, val: bool) {
                    unsafe {
                        let val: u8 = val as u8;
                        self._bitfield_1.as_mut().set(0usize, 1u8, val as u64)
                    }
                }
            })
        );
    }

    /// `usize` is its own type however wide it turns out to be, so it gets a
    /// written-out cast rather than being taken for the `u64` beside it.
    #[test]
    fn casts_rather_than_assuming_usize_is_a_fixed_width_type() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    impl Sizes {
                        #[inline]
                        pub fn sz(&self) -> usize {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(0usize, 8u8) as u64)
                            }
                        }
                    }
                }
            }
        };
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "sz"),
            expected_method(parse_quote! {
                #[inline]
                pub fn sz(&self) -> usize {
                    self._bitfield_1.get(0usize, 8u8) as u64 as usize
                }
            })
        );
    }

    /// rustc lints the type a `typedef` finally names, not the name the field
    /// was declared with, so the pass has to follow the mod's aliases to reach
    /// the same answer - through as many of them as it takes, and across the
    /// namespace the alias is declared in.
    #[test]
    fn follows_type_aliases_to_the_integer_underneath() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub mod detail {
                        pub type Handle = ::std::os::raw::c_int;
                    }
                    pub type Alias = root::detail::Handle;
                    pub type Flag = bool;
                    impl Aliased {
                        #[inline]
                        pub fn handle(&self) -> root::Alias {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(0usize, 4u8) as u32)
                            }
                        }
                        #[inline]
                        pub fn set_flag(&mut self, val: root::Flag) {
                            unsafe {
                                let val: u8 = ::std::mem::transmute(val);
                                self._bitfield_1.set(4usize, 1u8, val as u64)
                            }
                        }
                    }
                }
            }
        };
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "handle"),
            expected_method(parse_quote! {
                #[inline]
                pub fn handle(&self) -> root::Alias {
                    self._bitfield_1.get(0usize, 4u8) as u32 as root::Alias
                }
            })
        );
        assert_eq!(
            method_tokens(&m, "set_flag"),
            expected_method(parse_quote! {
                #[inline]
                pub fn set_flag(&mut self, val: root::Flag) {
                    let val: u8 = val as u8;
                    self._bitfield_1.set(4usize, 1u8, val as u64)
                }
            })
        );
    }

    /// A C++ `namespace raw` (or `ffi`) holding a type named like one of the
    /// C aliases is the user's own, and no integer: bindgen writes it as a
    /// relative path under `root`, where the real `c_int` is absolute. Taking
    /// it for an integer would emit `x as root::raw::c_int`, which is E0605 -
    /// a hard error on input that was perfectly valid.
    #[test]
    fn leaves_a_user_type_named_like_a_c_alias_alone() {
        let before: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub mod raw {
                        #[repr(u32)]
                        pub enum c_int {
                            ZERO = 0,
                        }
                    }
                    pub mod ffi {
                        #[repr(u32)]
                        pub enum c_uint {
                            ZERO = 0,
                        }
                    }
                    impl Confusing {
                        #[inline]
                        pub fn theirs(&self) -> root::raw::c_int {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(0usize, 2u8) as u32)
                            }
                        }
                        #[inline]
                        pub fn set_theirs(&mut self, val: root::ffi::c_uint) {
                            unsafe {
                                let val: u32 = ::std::mem::transmute(val);
                                self._bitfield_1.set(0usize, 2u8, val as u64)
                            }
                        }
                    }
                }
            }
        };
        let mut after = before.clone();
        simplify_bitfield_transmutes(&mut after);
        assert_eq!(tokens_of(&after), tokens_of(&before));
    }

    /// A header may define as many aliases as it likes, and following the
    /// chain only part of the way would leave the transmute - and the lint -
    /// exactly where they were. This chain is deliberately longer than any
    /// depth limit would plausibly have been.
    #[test]
    fn follows_an_alias_chain_of_any_length() {
        const LINKS: usize = 40;
        let mut aliases = TokenStream::new();
        aliases.extend(quote_alias("Alias0", "::std::os::raw::c_int"));
        for link in 1..LINKS {
            aliases.extend(quote_alias(
                &format!("Alias{link}"),
                &format!("root::Alias{}", link - 1),
            ));
        }
        let last = format!("root::Alias{}", LINKS - 1);
        let last: Type = syn::parse_str(&last).unwrap();
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    #aliases
                    impl Chained {
                        #[inline]
                        pub fn deep(&self) -> #last {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(0usize, 4u8) as u32)
                            }
                        }
                    }
                }
            }
        };
        simplify_bitfield_transmutes(&mut m);
        assert_eq!(
            method_tokens(&m, "deep"),
            expected_method(parse_quote! {
                #[inline]
                pub fn deep(&self) -> #last {
                    self._bitfield_1.get(0usize, 4u8) as u32 as #last
                }
            })
        );
    }

    fn quote_alias(name: &str, target: &str) -> TokenStream {
        let name: Ident = syn::parse_str(name).unwrap();
        let target: Type = syn::parse_str(target).unwrap();
        let alias: Item = parse_quote! { pub type #name = #target; };
        alias.to_token_stream()
    }

    /// A cycle of aliases names no type at all and could never compile, so
    /// resolution stops rather than spinning, and the accessor is left as
    /// bindgen wrote it.
    #[test]
    fn stops_at_a_cycle_of_aliases() {
        let before: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type Ouroboros = root::Tail;
                    pub type Tail = root::Ouroboros;
                    impl Cyclic {
                        #[inline]
                        pub fn round(&self) -> root::Ouroboros {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(0usize, 4u8) as u32)
                            }
                        }
                    }
                }
            }
        };
        let mut after = before.clone();
        simplify_bitfield_transmutes(&mut after);
        assert_eq!(tokens_of(&after), tokens_of(&before));
    }

    /// An alias to something no cast can produce is still left alone, however
    /// many aliases it took to find that out.
    #[test]
    fn leaves_an_alias_to_a_non_scalar_alone() {
        let before: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type Shade = root::Colour;
                    impl Aliased {
                        #[inline]
                        pub fn shade(&self) -> root::Shade {
                            unsafe {
                                ::std::mem::transmute(self._bitfield_1.get(0usize, 2u8) as u32)
                            }
                        }
                    }
                }
            }
        };
        let mut after = before.clone();
        simplify_bitfield_transmutes(&mut after);
        assert_eq!(tokens_of(&after), tokens_of(&before));
    }

    /// Nothing outside the shapes above is touched, however much it looks like
    /// a transmute we could simplify.
    #[test]
    fn leaves_transmutes_which_are_not_bitfield_accessors_alone() {
        let before: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    impl<T> __BindgenUnionField<T> {
                        #[inline]
                        pub unsafe fn as_ref(&self) -> &T {
                            unsafe { ::std::mem::transmute(self) }
                        }
                    }
                    impl Whatever {
                        // A getter shape, but the value comes from somewhere
                        // we know nothing about.
                        #[inline]
                        pub fn thing(&self) -> u8 {
                            unsafe { ::std::mem::transmute(elsewhere() as u8) }
                        }
                    }
                }
            }
        };
        let mut after = before.clone();
        simplify_bitfield_transmutes(&mut after);
        assert_eq!(tokens_of(&after), tokens_of(&before));
    }

    /// An expected method, rendered the same way `method_tokens` renders the
    /// real one so that the two are comparable.
    fn expected_method(f: syn::ImplItemFn) -> String {
        f.to_token_stream().to_string()
    }
}
