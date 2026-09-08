// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::conversion::apivec::ApiVec;
use crate::{conversion::ConvertErrorFromCpp, known_types::known_types};
use crate::{
    conversion::{
        analysis::tdef::TypedefPhase,
        api::{Api, TypedefKind},
        type_helpers::{
            is_pointer_like, strip_const_markers, unqualified_array_element_type, unwrap_bitfield,
            unwrap_function_pointer, unwrap_has_opaque,
        },
    },
    types::{Namespace, QualifiedName},
    ParseCallbackResults,
};
use autocxx_parser::IncludeCppConfig;
use std::collections::{HashMap, HashSet};
use syn::{ItemStruct, Type};

#[derive(Clone)]
enum PodState {
    UnsafeToBePod(String),
    SafeToBePod,
    IsPod,
    IsAlias(QualifiedName),
}

#[derive(Clone)]
struct StructDetails {
    state: PodState,
    dependent_structs: Vec<QualifiedName>,
}

impl StructDetails {
    fn new(state: PodState) -> Self {
        StructDetails {
            state,
            dependent_structs: Vec::new(),
        }
    }
}

/// Type which is able to check whether it's safe to make a type
/// fully representable by cxx. For instance if it is a struct containing
/// a struct containing a std::string, the answer is no, because that
/// std::string contains a self-referential pointer.
/// It is possible that this is duplicative of the information stored
/// elsewhere in the `Api` list and could possibly be removed or simplified.
/// In general this is one of the oldest parts of autocxx and
/// the code here could quite possibly be simplified by reusing code
/// elsewhere.
pub struct ByValueChecker {
    // Mapping from type name to whether it is safe to be POD
    results: HashMap<QualifiedName, StructDetails>,
    /// Types which inherit at least one base virtually. Such a type's layout
    /// is not the layout of the fields bindgen shows: the virtual base sits at
    /// an offset the object carries at runtime, and bindgen writes no field for
    /// the base. It writes one for the pointer which finds it only where the
    /// class needs a vtable pointer of its own - which `has_vtable` below
    /// already refuses on - and where the pointer comes from a base instead,
    /// all that is left is anonymous padding.
    virtually_inherited: HashSet<QualifiedName>,
    /// Types bindgen reported a base class for. Read together with the field
    /// bindgen writes in place of the base fields, so that a C++ member which
    /// happens to be spelled like that field cannot be mistaken for one.
    has_bases: HashSet<QualifiedName>,
}

impl ByValueChecker {
    pub fn new() -> Self {
        let mut results = HashMap::new();
        for (tn, by_value_safe) in known_types().get_pod_safe_types() {
            let safety = if by_value_safe {
                PodState::IsPod
            } else {
                PodState::UnsafeToBePod(format!("type {tn} is not safe for POD"))
            };
            results.insert(tn.clone(), StructDetails::new(safety));
        }
        ByValueChecker {
            results,
            virtually_inherited: HashSet::new(),
            has_bases: HashSet::new(),
        }
    }

    /// Scan APIs to work out which are by-value safe. Constructs a [ByValueChecker]
    /// that others can use to query the results.
    pub(crate) fn new_from_apis(
        apis: &ApiVec<TypedefPhase>,
        config: &IncludeCppConfig,
        parse_callback_results: &ParseCallbackResults,
    ) -> Result<ByValueChecker, ConvertErrorFromCpp> {
        let mut byvalue_checker = ByValueChecker::new();
        byvalue_checker.virtually_inherited = parse_callback_results
            .types_inheriting_virtually()
            .cloned()
            .collect();
        byvalue_checker.has_bases = parse_callback_results.types_with_bases().cloned().collect();
        for blocklisted in config.get_blocklist() {
            let tn = QualifiedName::new_from_cpp_name(blocklisted);
            let safety = PodState::UnsafeToBePod(format!("type {tn} is on the blocklist"));
            byvalue_checker
                .results
                .insert(tn, StructDetails::new(safety));
        }
        // As we do this analysis, we need to be aware that structs
        // may depend on other types. Ideally we'd use the depth first iterator
        // but that's awkward given that our ApiPhase does not yet have a fixed
        // list of field/base types. Instead, we'll iterate first over non-struct
        // types and then over structs, in an order we work out for ourselves.
        for api in apis.iter() {
            match api {
                Api::Typedef { analysis, .. } => {
                    let name = api.name();
                    // Whatever this typedef names: the substitute for a type we
                    // know about (`uint32_t` -> `u32`), or else the name as
                    // written, which may be another typedef or a struct we're
                    // also processing. Either way the typedef is POD exactly
                    // when its target is, so we record the link and let
                    // `satisfy_requests` walk the chain. See google/autocxx#264.
                    let typedef_target = match analysis.kind {
                        TypedefKind::Type(ref type_item) => match type_item.ty.as_ref() {
                            Type::Path(typ) => Some(QualifiedName::from_type_path(typ)),
                            _ => None,
                        },
                        TypedefKind::Use(ref ty) => match **ty {
                            crate::minisyn::Type(Type::Path(ref typ)) => {
                                Some(QualifiedName::from_type_path(typ))
                            }
                            _ => None,
                        },
                    }
                    .map(|target_tn| {
                        match known_types().consider_substitution(&target_tn) {
                            Some(typ) => QualifiedName::from_type_path(&typ),
                            None => target_tn,
                        }
                    });
                    // A typedef to a raw pointer is trivially
                    // copyable regardless of pointee, exactly like a
                    // directly written pointer field; previously it
                    // fell through to "typedef to a complex type" and
                    // poisoned containing PODs. See google/autocxx#1368.
                    // A C function pointer is the same story, and reaches us
                    // as `Option<unsafe extern "C" fn(..)>` rather than as a
                    // pointer, so it has to be spotted before the name of that
                    // `Option` is taken for the target of an alias.
                    // See google/autocxx#1494.
                    let target_is_pointer = match analysis.kind {
                        TypedefKind::Type(ref type_item) => is_pointer_like(type_item.ty.as_ref()),
                        TypedefKind::Use(ref ty) => is_pointer_like(ty),
                    };
                    match typedef_target {
                        _ if target_is_pointer => {
                            byvalue_checker
                                .results
                                .insert(name.clone(), StructDetails::new(PodState::IsPod));
                        }
                        Some(target) => {
                            byvalue_checker.results.insert(
                                name.clone(),
                                StructDetails::new(PodState::IsAlias(target)),
                            );
                        }
                        None => byvalue_checker.ingest_nonpod_type(name.clone()),
                    }
                }
                Api::Enum { .. } | Api::ExternCppType { pod: true, .. } => {
                    byvalue_checker
                        .results
                        .insert(api.name().clone(), StructDetails::new(PodState::IsPod));
                }
                _ => {}
            }
        }
        for (def, ns) in Self::structs_in_dependency_order(apis) {
            byvalue_checker.ingest_struct(def, ns)
        }
        // A `pod!` may name a nested type the way C++ does - `Outer::Inner` -
        // where we know it by the flattened `Outer_Inner` bindgen gave it, so
        // resolve through the names the APIs themselves answer to rather than
        // splitting the string and hoping. See google/autocxx#1422.
        let names_by_spelling: HashMap<String, QualifiedName> = apis
            .iter()
            .flat_map(|api| {
                let name = api.name();
                api.name_info()
                    .cpp_spellings()
                    .map(move |spelling| (spelling, name.clone()))
            })
            .collect();
        let pod_requests = config
            .get_pod_requests()
            .iter()
            .map(|ty| {
                names_by_spelling
                    .get(ty)
                    .cloned()
                    .unwrap_or_else(|| QualifiedName::new_from_cpp_name(ty))
            })
            .collect();
        byvalue_checker
            .satisfy_requests(pod_requests)
            .map_err(ConvertErrorFromCpp::UnsafePodType)?;
        Ok(byvalue_checker)
    }

    /// The structs among `apis`, ordered so that a struct comes after every
    /// struct it holds a field of.
    ///
    /// [`Self::ingest_struct`] decides a struct's POD-ness once and for all,
    /// looking each field type up in `results` as it goes, so a field type it
    /// has not reached yet counts as "isn't known" and poisons the struct
    /// permanently. Bindgen's own order is nearly always right, because C++
    /// requires a type to be complete before anything holds it by value and so
    /// the definition comes first - but not for a nested class, which bindgen
    /// hoists out to the same module as the class enclosing it and may emit
    /// afterwards. Sorting first is what makes the verdict independent of
    /// where the definition happened to land.
    ///
    /// Only fields naming another struct in this same list are followed:
    /// enums, typedefs and known types are settled by the pass before this
    /// one, and a pointer field is not a dependency at all. Held-by-value
    /// fields cannot form a cycle - a struct cannot contain itself - but if
    /// one ever appeared it would simply be emitted where it was reached
    /// rather than followed round for ever.
    fn structs_in_dependency_order(apis: &ApiVec<TypedefPhase>) -> Vec<(&ItemStruct, &Namespace)> {
        let structs: Vec<(&QualifiedName, &ItemStruct)> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Struct { details, .. } => Some((api.name(), &*details.item)),
                _ => None,
            })
            .collect();
        let positions: HashMap<&QualifiedName, usize> = structs
            .iter()
            .enumerate()
            .map(|(position, (name, _))| (*name, position))
            .collect();
        #[derive(Clone, Copy, PartialEq)]
        enum Progress {
            Unseen,
            /// Reached, and somewhere below us on the stack, so anything
            /// naming it again is naming a cycle.
            UnderWay,
            Ordered,
        }
        let mut order = Vec::with_capacity(structs.len());
        let mut progress = vec![Progress::Unseen; structs.len()];
        // An explicit stack rather than recursion: the depth is the depth of
        // the user's own type nesting, which nothing here bounds. The flag
        // says which of the two visits this is - on the way down, or back up
        // with every dependency already ordered.
        let mut stack = Vec::new();
        for root in 0..structs.len() {
            if progress[root] != Progress::Unseen {
                continue;
            }
            stack.push((root, false));
            while let Some((position, coming_back_up)) = stack.pop() {
                if coming_back_up {
                    progress[position] = Progress::Ordered;
                    let (name, def) = structs[position];
                    order.push((def, name.get_namespace()));
                    continue;
                }
                if progress[position] != Progress::Unseen {
                    // Ordered already, or reached again by a second route
                    // before we got to it.
                    continue;
                }
                progress[position] = Progress::UnderWay;
                // Back on the stack beneath everything it depends on, to be
                // ordered once they are all out of the way.
                stack.push((position, true));
                for field_type in Self::get_field_types(structs[position].1) {
                    if let Some(&dependency) = positions.get(&field_type) {
                        if progress[dependency] == Progress::Unseen {
                            stack.push((dependency, false));
                        }
                    }
                }
            }
        }
        order
    }

    fn ingest_struct(&mut self, def: &ItemStruct, ns: &Namespace) {
        // For this struct, work out whether it _could_ be safe as a POD.
        let tyname = QualifiedName::new(ns, def.ident.clone().into());
        let mut field_safety_problem = PodState::SafeToBePod;
        let fieldlist = Self::get_field_types(def);
        for ty_id in &fieldlist {
            match self.results.get(ty_id) {
                None if ty_id.get_final_item() == "__BindgenUnionField" => {
                    field_safety_problem = PodState::UnsafeToBePod(format!(
                        "Type {tyname} could not be POD because it is a union"
                    ));
                    break;
                }
                // A `long double` or `__float128` member. The Rust stand-in
                // for either has the right size and the wrong calling
                // convention - a struct of one 16-byte float is passed in an
                // SSE register where a struct of one 16-byte integer is not -
                // so a struct holding one cannot cross by value either. The
                // arm below would conclude the same thing anyway, naming the
                // marker rather than the type.
                None if matches!(
                    ty_id.get_final_item(),
                    "__bindgen_marker_LongDouble" | "__bindgen_marker_Float128"
                ) =>
                {
                    let cpp = if ty_id.get_final_item() == "__bindgen_marker_LongDouble" {
                        "long double"
                    } else {
                        "__float128"
                    };
                    field_safety_problem = PodState::UnsafeToBePod(format!(
                        "Type {tyname} could not be POD because it has a `{cpp}` member, \
                         which Rust has no type for - see the error for a `{cpp}` in a \
                         signature"
                    ));
                    break;
                }
                None => {
                    field_safety_problem = PodState::UnsafeToBePod(format!(
                        "Type {tyname} could not be POD because its dependent type {ty_id} isn't known"
                    ));
                    break;
                }
                Some(deets) => {
                    if let PodState::UnsafeToBePod(reason) = &deets.state {
                        let new_reason = format!("Type {tyname} could not be POD because its dependent type {ty_id} isn't safe to be POD. Because: {reason}");
                        field_safety_problem = PodState::UnsafeToBePod(new_reason);
                        break;
                    }
                }
            }
        }
        if Self::has_vtable(def) {
            let reason =
                format!("Type {tyname} could not be POD because it has virtual functions.");
            field_safety_problem = PodState::UnsafeToBePod(reason);
        }
        if self.has_bases.contains(&tyname) && Self::bases_are_opaque_bytes(def) {
            // The loop above already refused this - `__bindgen_marker_Opaque`
            // is not a type autocxx knows - but says so in terms of the marker
            // rather than of what happened. bindgen writes this one field in
            // place of the base fields when the target left the bases less
            // room than fields of their own types would take up, which is a
            // layout Rust cannot spell with a field per base; the bases are
            // bytes from here on, so nothing in the class can be read by
            // value. Asking whether the class has bases at all keeps a C++
            // member which happens to be spelled `__bindgen_bases` from being
            // read as one of those.
            let reason = format!(
                "Type {tyname} could not be POD because the C++ compiler leaves its base \
                 classes less room than fields of their own types would take up, so the bases \
                 have no Rust type."
            );
            field_safety_problem = PodState::UnsafeToBePod(reason);
        }
        if self.virtually_inherited.contains(&tyname) {
            // Not a conservative guess: a virtual base is at an offset the
            // object carries at runtime, so the fields above are not this
            // type's layout, and holding it by value in Rust would copy
            // something else. Nothing else here can see it - a virtual base
            // gets no field for `get_field_types` to find.
            let reason = format!(
                "Type {tyname} could not be POD because it inherits a base class virtually."
            );
            field_safety_problem = PodState::UnsafeToBePod(reason);
        }
        let mut my_details = StructDetails::new(field_safety_problem);
        my_details.dependent_structs = fieldlist;
        self.results.insert(tyname, my_details);
    }

    fn ingest_nonpod_type(&mut self, tyname: QualifiedName) {
        let new_reason = format!("Type {tyname} is a typedef to a complex type");
        self.results.insert(
            tyname,
            StructDetails::new(PodState::UnsafeToBePod(new_reason)),
        );
    }

    fn satisfy_requests(&mut self, mut requests: Vec<QualifiedName>) -> Result<(), String> {
        // Typedefs whose target hasn't settled yet, and which we've therefore
        // put back on the queue behind that target. Meeting the same typedef
        // here twice means its target still isn't settled after we asked for
        // it, i.e. the chain of typedefs is circular and never will settle, so
        // we must complain rather than spin round for ever.
        let mut aliases_awaiting_target: HashSet<QualifiedName> = HashSet::new();
        while let Some(ty_id) = requests.pop() {
            let deets = self.results.get_mut(&ty_id);
            let mut alias_to_consider = None;
            match deets {
                None => {
                    return Err(format!(
                        "Unable to make {ty_id} POD because we never saw a struct definition"
                    ))
                }
                Some(deets) => match &deets.state {
                    PodState::UnsafeToBePod(error_msg) => return Err(error_msg.clone()),
                    PodState::IsPod => {}
                    PodState::SafeToBePod => {
                        deets.state = PodState::IsPod;
                        requests.extend_from_slice(&deets.dependent_structs);
                    }
                    PodState::IsAlias(target_type) => {
                        alias_to_consider = Some(target_type.clone());
                    }
                },
            }
            // Do the following outside the match to avoid borrow checker violation.
            if let Some(alias) = alias_to_consider {
                match self.results.get(&alias).map(|deets| &deets.state) {
                    // The target's state is final, so this typedef is POD
                    // exactly when its target is. Adopt that state and go
                    // round again, which reports any error against the
                    // typedef in the normal way.
                    Some(state @ (PodState::IsPod | PodState::UnsafeToBePod(_))) => {
                        let state = match state {
                            PodState::UnsafeToBePod(reason) => PodState::UnsafeToBePod(format!(
                                "Type {ty_id} could not be POD because it is a typedef to {alias}. Because: {reason}"
                            )),
                            state => state.clone(),
                        };
                        self.results
                            .get_mut(&ty_id)
                            .expect("we matched on this entry a moment ago")
                            .state = state;
                        requests.push(ty_id);
                    }
                    // The target is a struct nobody has asked about yet, or
                    // another typedef: settle it first, then come back to
                    // this one. We pop from the back, so the target has to go
                    // on last.
                    Some(PodState::SafeToBePod | PodState::IsAlias(_)) => {
                        if !aliases_awaiting_target.insert(ty_id.clone()) {
                            return Err(format!(
                                "Unable to make {ty_id} POD because it is part of a circular chain of typedefs"
                            ));
                        }
                        requests.push(ty_id);
                        requests.push(alias);
                    }
                    // Every struct, enum and typedef we know of is already in
                    // `results` by now, so a target we can't find is one we
                    // never generated - blocklisted, or ignored earlier on.
                    None => {
                        return Err(format!(
                            "Unable to make {ty_id} POD because it is a typedef to {alias}, which we know nothing about"
                        ))
                    }
                }
            }
        }
        Ok(())
    }

    /// Return whether a given type is POD (i.e. can be represented by value in Rust) or not.
    /// Unless we've got a definite record that it _is_, we return false.
    /// Some types won't be in our `results` map. For example: (a) AutocxxConcrete types
    /// which we've synthesized; (b) types we couldn't parse but returned ignorable
    /// errors so that we could continue. Assume non-POD for all such cases.
    pub fn is_pod(&self, ty_id: &QualifiedName) -> bool {
        matches!(
            self.results.get(ty_id),
            Some(StructDetails {
                state: PodState::IsPod,
                dependent_structs: _,
            })
        )
    }

    /// This is a miniature version of the analysis in `super::get_struct_field_types`.
    /// It would be nice to unify them. However, this version only cares about spotting
    /// fields which may be non-POD, so can largely concern itself with the type a
    /// field names - through any number of array dimensions, since an array holds
    /// its elements by value.
    fn get_field_types(def: &ItemStruct) -> Vec<QualifiedName> {
        let mut results = Vec::new();
        for f in &def.fields {
            if f.ident
                .as_ref()
                .is_some_and(|id| id.to_string().starts_with("__bindgen_padding_"))
            {
                // Bytes bindgen inserted to reproduce the C++ layout. They
                // hold nothing, so they neither block POD-ness nor depend on
                // any type we'd have to settle first. Say so by name rather
                // than by type: what the field is for does not depend on which
                // shape bindgen chose to write the padding out in, and it has
                // written it as a blob and as a plain byte array at different
                // times.
                continue;
            }
            // `T arr[N]` holds N of `T` by value, so `T` decides whether the
            // field can be POD exactly as it would for a plain field of it -
            // with one exception, below.
            // A `const` field is laid out exactly as the type it qualifies,
            // so bindgen's marker for it decides nothing here; what it wraps
            // does.
            let field_ty = strip_const_markers(&f.ty);
            let field_is_array = matches!(field_ty, Type::Array(_));
            match unqualified_array_element_type(field_ty) {
                Type::Path(p) => {
                    if unwrap_bitfield(p).is_some() {
                        // A bitfield allocation unit is a byte array with
                        // accessors, so likewise.
                        continue;
                    }
                    if unwrap_function_pointer(p).is_some() {
                        // A C function pointer, which bindgen writes as
                        // `Option<unsafe extern "C" fn(..)>`. Copying the field
                        // copies a pointer, so it neither blocks POD-ness nor
                        // names a type we have to settle first - and the name it
                        // does bear, `std::option::Option`, is one we know nothing
                        // about. See google/autocxx#1494.
                        continue;
                    }
                    if field_is_array && unwrap_has_opaque(p).is_some() {
                        // An array of a type bindgen could not name, and so
                        // replaced by a blob of bytes of the right size and
                        // alignment. Arrays were invisible to this walk
                        // altogether until it learned to follow them, so an
                        // array of a blob has always been accepted here; a blob
                        // written as a plain field has always been refused,
                        // because the marker's name is one this knows nothing
                        // about. `test_array_of_blob_is_pod` and
                        // `test_plain_blob_field_is_not_pod` pin the two
                        // answers.
                        //
                        // The integration fixture which used to arrive here
                        // named its element through a using-declaration, which
                        // bindgen now resolves, so it no longer does - see
                        // `test_pod_array_of_concrete_instantiation_is_refused`,
                        // which is what that fixture became. The unit tests
                        // above are what pin this arm.
                        //
                        // Those two answers disagree, and reconciling them is
                        // not a question about arrays. A small blob is unwrapped
                        // to the integer of the same width, so making a POD of
                        // one hands safe Rust a writable field of a type whose
                        // C++ original may accept fewer values than the integer
                        // does - `bool` being the sharp case. Deciding whether
                        // to refuse both forms is therefore a soundness question
                        // about blobs, and is left exactly as it was rather than
                        // being settled as a side effect of following arrays.
                        continue;
                    }
                    results.push(QualifiedName::from_type_path(p));
                }
                // A pointer. Copying the field copies the pointer, whatever it
                // points at, so it neither blocks POD-ness nor names a type we
                // have to settle first. A C++ reference reaches us as a pointer
                // wrapped in `__bindgen_marker_Reference`, which is a path and
                // so goes through the arm above.
                Type::Ptr(_) => {}
                // Anything else contributes nothing. The only other shape
                // `TypeConverter::convert_type` accepts is a Rust reference,
                // which bindgen does not write for a field - it writes a
                // pointer inside `__bindgen_marker_Reference`, which is a path
                // and goes through the arm above - so in practice nothing
                // reaches here. A shape that did would be permitted into a POD
                // rather than refused, which is why the two arms above are
                // written out rather than folded into this one.
                _ => {}
            }
        }
        results
    }

    fn has_vtable(def: &ItemStruct) -> bool {
        for f in &def.fields {
            if f.ident.as_ref().map(|id| id == "vtable_").unwrap_or(false) {
                return true;
            }
        }
        false
    }

    /// Whether bindgen replaced this struct's base-class fields with the one
    /// opaque field it writes when the target's layout does not leave the
    /// bases room for their own types - see `third_party/patches/
    /// 17-base-class-extent.patch`.
    fn bases_are_opaque_bytes(def: &ItemStruct) -> bool {
        def.fields.iter().any(|f| {
            f.ident
                .as_ref()
                .map(|id| id == "__bindgen_bases")
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ByValueChecker, PodState, StructDetails};
    use crate::conversion::analysis::tdef::TypedefPhase;
    use crate::conversion::api::{Api, ApiName, StructDetails as ApiStructDetails};
    use crate::conversion::apivec::ApiVec;
    use crate::minisyn::ItemStruct;
    use crate::types::{Namespace, QualifiedName};
    use syn::parse_quote;

    fn ty_from_ident(id: &syn::Ident) -> QualifiedName {
        QualifiedName::new_from_cpp_name(&id.to_string())
    }

    /// An `Api::Struct` for `item`, in the global namespace, as the parse
    /// phase would have produced it.
    fn struct_api(item: ItemStruct) -> Api<TypedefPhase> {
        Api::Struct {
            name: ApiName::new(&Namespace::new(), item.ident.clone().into()),
            details: Box::new(ApiStructDetails {
                item,
                has_rvalue_reference_fields: false,
            }),
            analysis: (),
        }
    }

    /// The names of the structs of `apis`, in the order they'd be ingested.
    fn ingest_order(apis: &ApiVec<TypedefPhase>) -> Vec<String> {
        ByValueChecker::structs_in_dependency_order(apis)
            .into_iter()
            .map(|(def, _)| def.ident.to_string())
            .collect()
    }

    /// Record `name` as a typedef to `target`, as `new_from_apis` does for an
    /// `Api::Typedef`.
    fn add_alias(bvc: &mut ByValueChecker, name: &str, target: &str) -> QualifiedName {
        let name = QualifiedName::new_from_cpp_name(name);
        bvc.results.insert(
            name.clone(),
            StructDetails::new(PodState::IsAlias(QualifiedName::new_from_cpp_name(target))),
        );
        name
    }

    #[test]
    fn test_primitive_by_itself() {
        let bvc = ByValueChecker::new();
        let t_id = QualifiedName::new_from_cpp_name("u32");
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_primitives() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Foo {
                a: i32,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_nested_primitives() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Foo {
                a: i32,
                b: i64,
            }
        };
        bvc.ingest_struct(&t, &Namespace::new());
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: Foo,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_with_up() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: cxx::UniquePtr<CxxString>,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_with_cxxstring() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: CxxString,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        assert!(bvc.satisfy_requests(vec![t_id]).is_err());
    }

    #[test]
    fn test_typedef_chain_to_primitive() {
        let mut bvc = ByValueChecker::new();
        let first = add_alias(&mut bvc, "first", "u32");
        let second = add_alias(&mut bvc, "second", "first");
        let third = add_alias(&mut bvc, "third", "second");
        bvc.satisfy_requests(vec![third.clone()]).unwrap();
        assert!(bvc.is_pod(&third));
        assert!(bvc.is_pod(&second));
        assert!(bvc.is_pod(&first));
    }

    #[test]
    fn test_typedef_to_struct_makes_both_pod() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bob {
                a: u32,
            }
        };
        let bob = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        let horace = add_alias(&mut bvc, "Horace", "Bob");
        bvc.satisfy_requests(vec![horace.clone()]).unwrap();
        assert!(bvc.is_pod(&horace));
        // The struct behind the alias has to be POD as well, or we'd emit an
        // alias to an opaque type.
        assert!(bvc.is_pod(&bob));
    }

    #[test]
    fn test_typedef_to_non_pod_struct_is_rejected() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bob {
                a: CxxString,
            }
        };
        bvc.ingest_struct(&t, &Namespace::new());
        let horace = add_alias(&mut bvc, "Horace", "Bob");
        assert!(bvc.satisfy_requests(vec![horace]).is_err());
    }

    #[test]
    fn test_circular_typedefs_are_rejected() {
        // Such a cycle can't be written in C++, but we must terminate rather
        // than chase it round for ever if bindgen ever hands us one.
        let mut bvc = ByValueChecker::new();
        let a = add_alias(&mut bvc, "A", "B");
        add_alias(&mut bvc, "B", "A");
        let err = bvc.satisfy_requests(vec![a]).unwrap_err();
        assert!(
            format!("{err:?}").contains("circular"),
            "error should name the cycle, was: {err:?}"
        );
    }

    #[test]
    fn test_self_referential_typedef_is_rejected() {
        let mut bvc = ByValueChecker::new();
        let a = add_alias(&mut bvc, "A", "A");
        let err = bvc.satisfy_requests(vec![a]).unwrap_err();
        assert!(
            format!("{err:?}").contains("circular"),
            "error should name the cycle, was: {err:?}"
        );
    }

    #[test]
    fn test_typedef_to_unknown_type_is_rejected() {
        let mut bvc = ByValueChecker::new();
        let a = add_alias(&mut bvc, "A", "SomethingWeNeverSaw");
        let err = bvc.satisfy_requests(vec![a]).unwrap_err();
        assert!(
            format!("{err:?}").contains("SomethingWeNeverSaw"),
            "error should name the missing target, was: {err:?}"
        );
    }

    /// bindgen hoists a nested class out into the module its enclosing class
    /// is in, and may emit it after that class. Ingesting in that order would
    /// look the nested type up before anything had recorded it and rule the
    /// enclosing struct out of being POD for good, so the structs are sorted
    /// by what they hold before any of them is ingested.
    #[test]
    fn test_struct_holding_one_defined_after_it() {
        let mut apis = ApiVec::<TypedefPhase>::new();
        apis.push(struct_api(parse_quote! {
            struct Outer {
                inner: Inner,
            }
        }));
        apis.push(struct_api(parse_quote! {
            struct Inner {
                a: u32,
            }
        }));
        assert_eq!(ingest_order(&apis), vec!["Inner", "Outer"]);
    }

    /// A struct held by a struct held by the first one. The middle one has to
    /// be settled between them, whichever order they arrive in.
    #[test]
    fn test_chain_of_structs_defined_in_reverse() {
        let mut apis = ApiVec::<TypedefPhase>::new();
        apis.push(struct_api(parse_quote! {
            struct A {
                b: B,
            }
        }));
        apis.push(struct_api(parse_quote! {
            struct B {
                c: C,
            }
        }));
        apis.push(struct_api(parse_quote! {
            struct C {
                a: u32,
            }
        }));
        assert_eq!(ingest_order(&apis), vec!["C", "B", "A"]);
    }

    /// Two structs each holding the other cannot be written in C++, but we
    /// must still terminate, and still offer every struct for ingestion, if
    /// bindgen ever hands us one.
    #[test]
    fn test_structs_holding_each_other() {
        let mut apis = ApiVec::<TypedefPhase>::new();
        apis.push(struct_api(parse_quote! {
            struct A {
                b: B,
            }
        }));
        apis.push(struct_api(parse_quote! {
            struct B {
                a: A,
            }
        }));
        let mut order = ingest_order(&apis);
        order.sort();
        assert_eq!(order, vec!["A", "B"]);
    }

    /// An array of a POD struct is as POD as one of it, and asking for the
    /// holder makes the element type POD in turn - otherwise the holder would
    /// have a field of a type cxx treats as opaque.
    #[test]
    fn test_array_of_struct_makes_element_pod() {
        let mut bvc = ByValueChecker::new();
        let inner: ItemStruct = parse_quote! {
            struct Inner {
                a: u32,
            }
        };
        let inner_id = ty_from_ident(&inner.ident);
        bvc.ingest_struct(&inner, &Namespace::new());
        let outer: ItemStruct = parse_quote! {
            struct Outer {
                arr: [Inner; 4],
                b: i64,
            }
        };
        let outer_id = ty_from_ident(&outer.ident);
        bvc.ingest_struct(&outer, &Namespace::new());
        bvc.satisfy_requests(vec![outer_id.clone()]).unwrap();
        assert!(bvc.is_pod(&outer_id));
        assert!(bvc.is_pod(&inner_id));
    }

    /// An array of something which can't be held by value in Rust makes the
    /// struct holding it just as unsafe as a plain field of it would. The
    /// element type has to be spelled the way the known-types database does,
    /// or the refusal would come from not recognising the name at all and the
    /// test would pass for any element type whatsoever.
    #[test]
    fn test_array_of_cxxstring() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: [cxx::CxxString; 4],
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        let err = bvc.satisfy_requests(vec![t_id]).unwrap_err();
        assert!(
            err.contains("isn't safe to be POD"),
            "should be refused for being unsafe, not for being unknown, was: {err}"
        );
    }

    /// C++ nests arrays for `T arr[2][3]`, so the walk has to peel off however
    /// many dimensions there are rather than just the one.
    #[test]
    fn test_array_of_arrays_of_cxxstring() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: [[cxx::CxxString; 3]; 2],
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        let err = bvc.satisfy_requests(vec![t_id]).unwrap_err();
        assert!(
            err.contains("isn't safe to be POD"),
            "should be refused for being unsafe, not for being unknown, was: {err}"
        );
    }

    /// An array of a type bindgen could not name, and so replaced with a blob
    /// of bytes, leaves the struct holding it POD. See `get_field_types`, where
    /// this and the next test are the two halves of the asymmetry it describes.
    #[test]
    fn test_array_of_blob_is_pod() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                arr: [__bindgen_marker_Opaque<u32>; 4usize],
                tail: u32,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    /// The same blob written as a plain field is refused, because the marker
    /// names a type this knows nothing about.
    #[test]
    fn test_plain_blob_field_is_not_pod() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                one: __bindgen_marker_Opaque<u32>,
                tail: u32,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        let err = bvc.satisfy_requests(vec![t_id]).unwrap_err();
        assert!(
            err.contains("__bindgen_marker_Opaque"),
            "should be refused for naming a type we don't know, was: {err}"
        );
    }

    /// The element type of an array is a dependency for ordering purposes too,
    /// so a struct holding an array of one defined after it still has to be
    /// ingested second.
    #[test]
    fn test_struct_holding_array_of_one_defined_after_it() {
        let mut apis = ApiVec::<TypedefPhase>::new();
        apis.push(struct_api(parse_quote! {
            struct Outer {
                inner: [Inner; 4],
            }
        }));
        apis.push(struct_api(parse_quote! {
            struct Inner {
                a: u32,
            }
        }));
        assert_eq!(ingest_order(&apis), vec!["Inner", "Outer"]);
    }

    /// A bitfield allocation unit is a byte array with accessors, so it
    /// doesn't stop the struct holding it being POD - nor does the padding
    /// bindgen adds alongside, whose type nobody else has heard of.
    #[test]
    fn test_with_bitfield() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Foo {
                a: root::__BindgenBitfieldUnit<[u8; 4usize]>,
                b: i64,
                __bindgen_padding_0: __bindgen_marker_Opaque<u16>,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }
}
