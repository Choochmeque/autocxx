// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use proc_macro2::TokenStream;
use syn::{Expr, Ident, Path, Type};

use crate::{
    conversion::analysis::fun::function_wrapper::{
        BridgePointer, PointerRustConversion, TypeConversionPolicy, WholeRustConversion,
    },
    types::make_ident,
};
use quote::quote;
use syn::parse_quote;

use super::MaybeUnsafeStmt;

/// Output Rust snippets for how to deal with a given parameter.
pub(super) enum RustParamConversion {
    Param {
        ty: Type,
        local_variables: Vec<MaybeUnsafeStmt>,
        conversion: TokenStream,
        conversion_requires_unsafe: bool,
    },
    ReturnValue {
        ty: Type,
    },
}

impl TypeConversionPolicy {
    /// How this parameter crosses in the direction opposite to the one
    /// [`Self::rust_conversion`] describes - from C++ into Rust, which is what
    /// a `subclass!` override is called in.
    ///
    /// `None` means the parameter arrives in Rust as whatever the bridge says,
    /// and there is nothing to undo. That is every conversion but the
    /// reference wrappers of `ReferencesWrappedAllFunctionsSafe`: those turn a
    /// C++ reference into a pointer on the way out, so on the way in the
    /// pointer has to become a wrapper again, or the Rust which implements the
    /// override would be handed a raw pointer by a mode whose whole purpose is
    /// not to.
    ///
    /// The wrapper is the lifetime-free `autocxx::CppRef` rather than a
    /// `CppLtRef`: there is no Rust value here for a borrow to be tied to, only
    /// a reference C++ chose to pass, and its validity is C++'s promise for the
    /// duration of the call.
    ///
    /// Returns the type the parameter arrives as, and the path whose `from_ptr`
    /// makes one out of what the bridge hands over.
    pub(super) fn inverse_rust_conversion(&self) -> Option<(Type, Path)> {
        match self {
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromReferenceWrapperToPointer,
                ..
            } => {
                let ty = pointer.pointee();
                Some(if pointer.is_mut() {
                    (
                        parse_quote! { autocxx::CppMutRef<#ty> },
                        parse_quote! { autocxx::CppMutRef },
                    )
                } else {
                    (
                        parse_quote! { autocxx::CppRef<#ty> },
                        parse_quote! { autocxx::CppRef },
                    )
                })
            }
            _ => None,
        }
    }

    /// As [`Self::inverse_rust_conversion`], but for the value a function
    /// hands back rather than one it is given, so the two directions are the
    /// other way round: [`Self::rust_conversion`] describes C++ returning to
    /// Rust, and this describes Rust returning to C++, which is what a
    /// `subclass!` override does.
    ///
    /// `None` for every conversion but the reference wrappers of
    /// `ReferencesWrappedAllFunctionsSafe`. Those turn the pointer the bridge
    /// returns into a wrapper for Rust to receive; here the override produces
    /// the wrapper and the bridge has to be handed the pointer back out of it,
    /// or a mode whose whole purpose is to keep raw pointers out of Rust would
    /// be asking the override to conjure one.
    ///
    /// The wrapper is the lifetime-free `autocxx::CppRef` rather than a
    /// `CppLtRef`, for the same reason as in the parameter direction and one
    /// more: what the override hands back goes straight to C++, which is under
    /// no obligation to a Rust lifetime, so a borrow of `&self` here would
    /// constrain the Rust which implements the override without protecting
    /// anything. The contract is C++'s own, and unchanged by the wrapper: as
    /// for any C++ virtual method, the reference returned must outlive the
    /// call.
    ///
    /// Returns the type the override hands back, and the method which gets the
    /// pointer the bridge must return out of one.
    pub(super) fn inverse_rust_return_conversion(&self) -> Option<(Type, Ident)> {
        match self {
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromPointerToReferenceWrapper,
                ..
            } => {
                let ty = pointer.pointee();
                Some(if pointer.is_mut() {
                    (
                        parse_quote! { autocxx::CppMutRef<#ty> },
                        make_ident("as_mut_ptr").into(),
                    )
                } else {
                    (
                        parse_quote! { autocxx::CppRef<#ty> },
                        make_ident("as_ptr").into(),
                    )
                })
            }
            _ => None,
        }
    }

    pub(super) fn rust_conversion(&self, var: Expr, counter: &mut usize) -> RustParamConversion {
        match self {
            Self::Pointer {
                rust: PointerRustConversion::None,
                ..
            }
            | Self::Whole {
                rust: WholeRustConversion::None,
                ..
            } => RustParamConversion::Param {
                ty: self.converted_rust_type(),
                local_variables: Vec::new(),
                conversion: quote! { #var },
                conversion_requires_unsafe: false,
            },
            Self::Whole {
                rust: WholeRustConversion::FromStr,
                ..
            } => RustParamConversion::Param {
                ty: parse_quote! { impl ToCppString },
                local_variables: Vec::new(),
                conversion: quote! ( #var .into_cpp() ),
                conversion_requires_unsafe: false,
            },
            Self::Whole {
                rust: WholeRustConversion::FromBytes,
                ..
            } => RustParamConversion::Param {
                ty: parse_quote! { impl AsCppStringView },
                local_variables: Vec::new(),
                conversion: quote! ( #var .as_string_view_bytes() ),
                conversion_requires_unsafe: false,
            },
            Self::Whole {
                rust: WholeRustConversion::ToBoxedUpHolder(sub),
                ..
            } => {
                let holder_type = sub.holder();
                let id = sub.id();
                let ty = parse_quote! { autocxx::subclass::CppSubclassRustPeerHolder<
                    super:: #id>
                };
                RustParamConversion::Param {
                    ty,
                    local_variables: Vec::new(),
                    conversion: quote! {
                        Box::new(#holder_type(#var))
                    },
                    conversion_requires_unsafe: false,
                }
            }
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromPinMaybeUninitToPtr,
                ..
            } => {
                let ty = pointer.pointee();
                let ty = parse_quote! {
                    ::core::pin::Pin<&mut ::core::mem::MaybeUninit< #ty >>
                };
                RustParamConversion::Param {
                    ty,
                    local_variables: Vec::new(),
                    conversion: quote! {
                        #var.get_unchecked_mut().as_mut_ptr()
                    },
                    conversion_requires_unsafe: true,
                }
            }
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromPinMoveRefToPtr,
                ..
            } => {
                let ty = pointer.pointee();
                let ty = parse_quote! {
                    ::core::pin::Pin<autocxx::moveit::MoveRef< '_, #ty >>
                };
                RustParamConversion::Param {
                    ty,
                    local_variables: Vec::new(),
                    conversion: quote! {
                        { let r: &mut _ = ::core::pin::Pin::into_inner_unchecked(#var.as_mut());
                            r
                        }
                    },
                    conversion_requires_unsafe: true,
                }
            }
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromTypeToPtr,
                ..
            } => {
                let ty = pointer.pointee();
                let ty = parse_quote! { &mut #ty };
                RustParamConversion::Param {
                    ty,
                    local_variables: Vec::new(),
                    conversion: quote! {
                        #var
                    },
                    conversion_requires_unsafe: false,
                }
            }
            Self::Whole {
                rust: WholeRustConversion::FromValueParamToPtr,
                ..
            } => self.param_handler(var, counter, "ValueParamHandler", "ValueParam"),
            Self::Whole {
                rust: WholeRustConversion::FromRValueParamToPtr,
                ..
            } => self.param_handler(var, counter, "RValueParamHandler", "RValueParam"),
            // This type of conversion means that this function parameter appears in the cxx::bridge
            // but not in the arguments for the wrapper function, because instead we return an
            // impl New which uses the cxx::bridge function's pointer parameter.
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromPlacementParamToNewReturn,
                ..
            } => RustParamConversion::ReturnValue {
                ty: pointer.pointee().clone(),
            },
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromPointerToReferenceWrapper,
                ..
            } => {
                let ty = pointer.pointee();
                let (ty, wrapper_name) = if pointer.is_mut() {
                    (
                        parse_quote! { autocxx::CppMutLtRef<'a, #ty> },
                        "CppMutLtRef",
                    )
                } else {
                    (parse_quote! { autocxx::CppLtRef<'a, #ty> }, "CppLtRef")
                };
                let wrapper_name = make_ident(wrapper_name);
                RustParamConversion::Param {
                    ty,
                    local_variables: Vec::new(),
                    conversion: quote! {
                        autocxx::#wrapper_name::from_ptr (#var)
                    },
                    conversion_requires_unsafe: false,
                }
            }
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromReferenceWrapperToPointer,
                ..
            } => {
                let is_mut = pointer.is_mut();
                let ty = pointer.pointee();
                let ty = if is_mut {
                    parse_quote! { autocxx::CppMutRef<#ty> }
                } else {
                    parse_quote! { autocxx::CppRef<#ty> }
                };
                RustParamConversion::Param {
                    ty,
                    local_variables: Vec::new(),
                    conversion: if is_mut {
                        quote! {
                            #var .as_mut_ptr()
                        }
                    } else {
                        quote! {
                            #var .as_ptr()
                        }
                    },
                    conversion_requires_unsafe: false,
                }
            }
            // Handing the address back to C++, which qualified what it points
            // at `volatile` and will do the access itself. Unwrapping is all
            // that is needed, and it reads nothing.
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromVolatilePtrToPointer,
                ..
            } => RustParamConversion::Param {
                ty: volatile_handle_type(pointer),
                local_variables: Vec::new(),
                conversion: quote! {
                    #var .as_raw()
                },
                conversion_requires_unsafe: false,
            },
            // The other direction: C++ produced the address of storage it
            // qualified `volatile`, and Rust is the one which will perform the
            // accesses. Wrapping it is what stops those being ordinary loads.
            Self::Pointer {
                pointer,
                rust: PointerRustConversion::FromPointerToVolatilePtr,
                ..
            } => {
                // The bare path, not the type: `VolatilePtr<T>::new` is not an
                // expression, and the pointee follows from the argument.
                let handle_name = make_ident(if pointer.is_mut() {
                    "VolatilePtr"
                } else {
                    "VolatileConstPtr"
                });
                RustParamConversion::Param {
                    ty: volatile_handle_type(pointer),
                    local_variables: Vec::new(),
                    conversion: quote! {
                        autocxx::#handle_name::new(#var)
                    },
                    conversion_requires_unsafe: false,
                }
            }
        }
    }

    /// The conversion shared by the two by-value parameter kinds: put a
    /// handler on the stack, populate it with the caller's value, and hand the
    /// C++ side the pointer it hands back.
    fn param_handler(
        &self,
        var: Expr,
        counter: &mut usize,
        handler_type: &str,
        param_trait: &str,
    ) -> RustParamConversion {
        let handler_type = make_ident(handler_type);
        let param_trait = make_ident(param_trait);
        let var_counter = *counter;
        *counter += 1;
        let space_var_name = format!("space{var_counter}");
        let space_var_name = make_ident(space_var_name);
        let ty = self.cxxbridge_type();
        let ty = parse_quote! { impl autocxx::#param_trait<#ty> };
        // This is the usual trick to put something on the stack, then
        // immediately shadow the variable name so it can't be accessed or moved.
        RustParamConversion::Param {
            ty,
            local_variables: vec![
                MaybeUnsafeStmt::new(
                    quote! { let mut #space_var_name = autocxx::#handler_type::default(); },
                ),
                MaybeUnsafeStmt::binary(
                    quote! { let mut #space_var_name =
                        unsafe { ::core::pin::Pin::new_unchecked(&mut #space_var_name) };
                    },
                    quote! { let mut #space_var_name =
                        ::core::pin::Pin::new_unchecked(&mut #space_var_name);
                    },
                ),
                MaybeUnsafeStmt::needs_unsafe(quote! { #space_var_name.as_mut().populate(#var); }),
            ],
            conversion: quote! {
                #space_var_name.get_ptr()
            },
            conversion_requires_unsafe: false,
        }
    }
}

/// The handle autocxx hands over for an address whose pointee C++ qualified
/// `volatile`. A `const volatile` pointee gets the read-only one: writing
/// through a pointer to a `const` object is undefined however it is written.
fn volatile_handle_type(pointer: &BridgePointer) -> Type {
    let ty = pointer.pointee();
    if pointer.is_mut() {
        parse_quote! { autocxx::VolatilePtr<#ty> }
    } else {
        parse_quote! { autocxx::VolatileConstPtr<#ty> }
    }
}
