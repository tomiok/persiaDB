//! `cargo xtask public-api` over small fixture crates, and the committed snapshot's content (ROADMAP 10.1.12,
//! 10.1.16).
#![allow(clippy::needless_pass_by_value, clippy::too_many_lines)] // Fixtures: builders take the values they place.

use serde_json::{Value as J, json};

use super::render::FORMAT_VERSION;
use super::{Crate, Crates, SNAPSHOT, diff, render};

/// A crate `fix` with a `#[non_exhaustive]` struct `Foo` (a public and a private field, an inherent impl with a
/// public and a private method, a `Clone` impl and a blanket impl), a private function, and a re-export of
/// `other::Bar`. `extra` is added to `Foo`'s inherent impl.
fn fix(extra: &[(u64, J)]) -> J {
    let mut index = json!({
        "0": {"id": 0, "name": "fix", "visibility": "public", "attrs": [],
              "inner": {"module": {"is_crate": true, "items": [1, 6, 7], "is_stripped": false}}},
        "1": {"id": 1, "name": "Foo", "visibility": "public", "attrs": ["non_exhaustive"],
              "inner": {"struct": {"kind": {"plain": {"fields": [2, 8, 15], "has_stripped_fields": true}},
                                   "generics": {"params": [], "where_predicates": []}, "impls": [3, 10, 11]}}},
        "2": {"id": 2, "name": "x", "visibility": "public", "attrs": [], "inner": {"struct_field": {"primitive": "u32"}}},
        "8": {"id": 8, "name": "hidden", "visibility": "default", "attrs": [],
              "inner": {"struct_field": {"primitive": "u8"}}},
        "15": {"id": 15, "name": "y", "visibility": "public", "attrs": [],
               "inner": {"struct_field": {"resolved_path": {"path": "Bar", "id": 9, "args": null}}}},
        "3": {"id": 3, "name": null, "visibility": "default", "attrs": [],
              "inner": {"impl": {"is_unsafe": false, "generics": {"params": [], "where_predicates": []},
                                 "trait": null, "for": {"resolved_path": {"path": "Foo", "id": 1, "args": null}},
                                 "items": [4, 5], "is_negative": false, "is_synthetic": false, "blanket_impl": null}}},
        "4": function("new", true, &json!([]), Some(&json!({"generic": "Self"}))),
        "5": function("private", false, &json!([]), None),
        "10": {"id": 10, "name": null, "visibility": "default", "attrs": [],
               "inner": {"impl": {"is_unsafe": false, "generics": {"params": [], "where_predicates": []},
                                  "trait": {"path": "Clone", "id": 20, "args": null},
                                  "for": {"resolved_path": {"path": "Foo", "id": 1, "args": null}},
                                  "items": [], "is_negative": false, "is_synthetic": false, "blanket_impl": null}}},
        "11": {"id": 11, "name": null, "visibility": "default", "attrs": [],
               "inner": {"impl": {"is_unsafe": false, "generics": {"params": [], "where_predicates": []},
                                  "trait": {"path": "From", "id": 21, "args": null},
                                  "for": {"generic": "T"}, "items": [], "is_negative": false, "is_synthetic": false,
                                  "blanket_impl": {"generic": "T"}}}},
        "6": function("helper", false, &json!([]), None),
        "7": {"id": 7, "name": null, "visibility": "public", "attrs": [],
              "inner": {"use": {"source": "other::Bar", "name": "Bar", "id": 9, "is_glob": false}}},
    });
    let impl_items = index
        .pointer_mut("/3/inner/impl/items")
        .and_then(J::as_array_mut)
        .unwrap();
    for (id, _) in extra {
        impl_items.push(json!(id));
    }
    let object = index.as_object_mut().unwrap();
    for (id, item) in extra {
        object.insert(id.to_string(), item.clone());
    }
    json!({
        "root": 0, "format_version": FORMAT_VERSION, "index": index,
        "paths": {
            "0": {"crate_id": 0, "path": ["fix"], "kind": "module"},
            "1": {"crate_id": 0, "path": ["fix", "inner", "Foo"], "kind": "struct"},
            "9": {"crate_id": 1, "path": ["other", "Bar"], "kind": "struct"},
            "20": {"crate_id": 2, "path": ["core", "clone", "Clone"], "kind": "trait"},
            "21": {"crate_id": 2, "path": ["core", "convert", "From"], "kind": "trait"},
        },
        "external_crates": {"1": {"name": "other"}, "2": {"name": "core"}},
    })
}

/// A method item `name(&self, extra_inputs...)`, public or not, returning `output`.
fn function(name: &str, public: bool, extra_inputs: &J, output: Option<&J>) -> J {
    let mut inputs = vec![
        json!(["self", {"borrowed_ref": {"lifetime": null, "is_mutable": false,
                                                            "type": {"generic": "Self"}}}]),
    ];
    inputs.extend(extra_inputs.as_array().cloned().unwrap_or_default());
    json!({"id": 0, "name": name, "visibility": if public { "public" } else { "default" }, "attrs": [],
           "inner": {"function": {"sig": {"inputs": inputs, "output": output, "is_c_variadic": false},
                                  "generics": {"params": [], "where_predicates": []},
                                  "header": {"is_const": false, "is_unsafe": false, "is_async": false, "abi": "Rust"},
                                  "has_body": true}}})
}

/// The crate `other`, whose struct `Bar(u64)` `fix` re-exports, with a method `get(&self) -> u64`.
fn other() -> J {
    json!({
        "root": 0, "format_version": FORMAT_VERSION,
        "index": {
            "0": {"id": 0, "name": "other", "visibility": "public", "attrs": [],
                  "inner": {"module": {"is_crate": true, "items": [9], "is_stripped": false}}},
            "9": {"id": 9, "name": "Bar", "visibility": "public", "attrs": [],
                  "inner": {"struct": {"kind": {"tuple": [12]}, "generics": {"params": [], "where_predicates": []},
                                       "impls": [13]}}},
            "12": {"id": 12, "name": "0", "visibility": "public", "attrs": [],
                   "inner": {"struct_field": {"primitive": "u64"}}},
            "13": {"id": 13, "name": null, "visibility": "default", "attrs": [],
                   "inner": {"impl": {"is_unsafe": false, "generics": {"params": [], "where_predicates": []},
                                      "trait": null, "for": {"resolved_path": {"path": "Bar", "id": 9, "args": null}},
                                      "items": [14], "is_negative": false, "is_synthetic": false,
                                      "blanket_impl": null}}},
            "14": function("get", true, &json!([]), Some(&json!({"primitive": "u64"}))),
        },
        // "15" sorts first: `find` must match the path, not take the first item of this crate.
        "paths": {"15": {"crate_id": 0, "path": ["other", "Unrelated"], "kind": "struct"},
                  "9": {"crate_id": 0, "path": ["other", "Bar"], "kind": "struct"}},
        "external_crates": {},
    })
}

fn rendered(fix: &J) -> String {
    let crates = Crates::new(
        Crate::parse(&fix.to_string()).unwrap(),
        vec![Crate::parse(&other().to_string()).unwrap()],
    )
    .unwrap();
    render(&crates).unwrap()
}

#[test]
fn a_fixture_crate_renders_its_public_items_sorted_with_its_re_exports_expanded() {
    assert_eq!(
        rendered(&fix(&[])),
        "#[non_exhaustive] pub struct fix::Foo\n\
         impl core::clone::Clone for fix::Foo\n\
         pub field fix::Foo::x: u32\n\
         pub field fix::Foo::y: fix::Bar\n\
         pub fn fix::Bar::get(&self) -> u64\n\
         pub fn fix::Foo::new(&self) -> Self\n\
         pub struct fix::Bar(u64)\n"
    );
}

/// ROADMAP 10.1.12 [T]: a public item added without regenerating the snapshot fails the check, which names it; a
/// private one does not change the snapshot.
#[test]
fn a_public_item_added_without_regenerating_fails_the_check() {
    let committed = rendered(&fix(&[]));
    assert_eq!(diff(&committed, &committed), None);
    let private = fix(&[(30, function("internal", false, &json!([]), None))]);
    assert_eq!(diff(&committed, &rendered(&private)), None);
    let added = fix(&[(
        30,
        function(
            "len",
            true,
            &json!([["at", {"primitive": "usize"}]]),
            Some(&json!({"primitive": "usize"})),
        ),
    )]);
    let report = diff(&committed, &rendered(&added)).expect("the check fails");
    assert_eq!(
        report,
        "+ pub fn fix::Foo::len(&self, at: usize) -> usize\n"
    );
    // And the other way: an item removed is reported as such.
    let report = diff(&rendered(&added), &committed).expect("the check fails");
    assert_eq!(
        report,
        "- pub fn fix::Foo::len(&self, at: usize) -> usize\n"
    );
}

#[test]
fn a_snapshot_with_the_same_lines_in_another_order_fails_the_check() {
    let committed = rendered(&fix(&[]));
    let mut lines: Vec<&str> = committed.lines().collect();
    lines.reverse();
    let shuffled = format!("{}\n", lines.join("\n"));
    assert!(diff(&shuffled, &committed).is_some_and(|r| r.contains("another order")));
}

#[test]
fn json_of_another_format_version_is_refused() {
    let mut json = fix(&[]);
    json["format_version"] = json!(FORMAT_VERSION + 1);
    let err = Crate::parse(&json.to_string()).err().expect("refused");
    assert!(err.to_string().contains("format"), "{err}");
}

/// The committed snapshot (ROADMAP 10.1.16 [T]): the engine detail trimmed from `persia`'s surface is not in it, what
/// was kept on purpose is, and it covers the feature `async` but not the hidden `internal` layer or the test hooks.
#[test]
fn the_committed_snapshot_lists_only_what_is_kept_on_purpose() {
    let root = crate::repo_root().unwrap();
    let snapshot = std::fs::read_to_string(root.join(SNAPSHOT)).unwrap();
    for hidden in [
        "persia::CollectionSchema::hash(",
        "persia::CollectionSchema::hash_at(",
        "persia::CollectionSchema::canonical_index_definition(",
        "persia::CollectionSchema::from_json_in(",
        "persia::AnalyzerRef::parse_in(",
        "persia::Document::encode(",
        "persia::Document::decode(",
        "persia::Document::encoded_id(",
        "AnalyzerRegistry",
        "SchemaHash",
        "persia::internal",
        "maintain_now",
        "is_pack_mapped",
        "DetachedSlot",
        "ChunkSize",
        "persia_blob::",
    ] {
        assert!(!snapshot.contains(hidden), "{hidden} is in {SNAPSHOT}");
    }
    for kept in [
        "pub fn persia::Document::extra(",
        "pub fn persia::Document::insert_extra(",
        "#[non_exhaustive] pub struct persia::TextOptions",
        "#[non_exhaustive] pub enum persia::FieldType",
        "pub struct persia::AsyncDb",
        "pub fn persia::SearchResponse::to_json(&self) -> persia::Result<alloc::string::String>",
        "impl serde_core::ser::Serialize for persia::SearchResponse",
    ] {
        assert!(snapshot.contains(kept), "{kept} is not in {SNAPSHOT}");
    }
}

/// A resolved path to `id` (canonical `fix::...` or `core::...` in [`rich`]'s `paths`) with angle-bracketed `args`.
fn path(name: &str, id: u64, args: &[J]) -> J {
    let args = if args.is_empty() {
        J::Null
    } else {
        json!({"angle_bracketed": {"args": args.iter().map(|a| json!({"type": a})).collect::<Vec<_>>(),
                                   "constraints": []}})
    };
    json!({"path": name, "id": id, "args": args})
}

fn bound(trait_: J, modifier: &str) -> J {
    json!({"trait_bound": {"trait": trait_, "generic_params": [], "modifier": modifier}})
}

fn impl_item(
    id: u64,
    trait_: J,
    synthetic: bool,
    negative: bool,
    unsafe_: bool,
    generics: J,
) -> (String, J) {
    (
        id.to_string(),
        json!({"id": id, "name": null, "visibility": "default", "attrs": [],
               "inner": {"impl": {"is_unsafe": unsafe_, "generics": generics, "trait": trait_,
                                  "for": {"resolved_path": path("Gen", 1, &[json!({"generic": "T"})])},
                                  "items": [], "is_negative": negative, "is_synthetic": synthetic,
                                  "blanket_impl": null}}}),
    )
}

/// A crate whose items reach every type, bound, generic and impl form the renderer writes.
fn rich() -> J {
    let t = json!({"generic": "T"});
    let u8_ = json!({"primitive": "u8"});
    let no_generics = json!({"params": [], "where_predicates": []});
    let fn_u8 = json!({"path": "Fn", "id": 22, "args": {"parenthesized": {"inputs": [u8_], "output": u8_}}});
    let iter_item = json!({"path": "Iterator", "id": 23, "args": {"angle_bracketed": {"args": [],
        "constraints": [{"name": "Item", "args": null, "binding": {"equality": {"type": u8_}}}]}}});
    let sig_inputs = json!([
        ["self", {"borrowed_ref": {"lifetime": "'b", "is_mutable": true, "type": {"generic": "Self"}}}],
        ["a", {"borrowed_ref": {"lifetime": "'a", "is_mutable": false, "type": {"slice": t}}}],
        ["b", {"array": {"type": u8_, "len": "N"}}],
        ["c", {"tuple": [u8_]}],
        ["d", {"tuple": [u8_, {"primitive": "i8"}]}],
        ["e", {"raw_pointer": {"is_mutable": false, "type": u8_}}],
        ["f", {"raw_pointer": {"is_mutable": true, "type": u8_}}],
        ["g", {"impl_trait": [bound(fn_u8.clone(), "none")]}],
        ["h", {"resolved_path": path("Box", 24, &[json!({"dyn_trait": {"traits": [
            {"trait": {"path": "Send", "id": 25, "args": null}, "generic_params": []}], "lifetime": "'a"}})])}],
        ["i", {"qualified_path": {"name": "Item", "args": null, "self_type": t,
                                  "trait": {"path": "Iterator", "id": 23, "args": null}}}],
        ["j", {"function_pointer": {"sig": {"inputs": [["_", u8_]], "output": u8_, "is_c_variadic": false},
                                    "generic_params": [], "header": {}}}],
        ["k", {"resolved_path": path("Option", 26, &[json!("infer")])}],
        ["l", {"tuple": []}],
    ]);
    let generics = json!({"params": [
        {"name": "'b", "kind": {"lifetime": {"outlives": ["'a"]}}},
        {"name": "U", "kind": {"type": {"bounds": [bound(json!({"path": "Clone", "id": 20, "args": null}), "none"),
                                                   bound(json!({"path": "Sized", "id": 27, "args": null}), "maybe"),
                                                   bound(json!({"path": "Clone", "id": 20, "args": null}), "maybe_const")],
                                        "default": null, "is_synthetic": false}}},
        {"name": "impl Fn(u8) -> u8", "kind": {"type": {"bounds": [], "default": null, "is_synthetic": true}}},
        {"name": "N", "kind": {"const": {"type": {"primitive": "usize"}, "default": null}}},
    ], "where_predicates": [
        {"bound_predicate": {"type": t, "bounds": [bound(iter_item, "none")], "generic_params": []}},
        {"lifetime_predicate": {"lifetime": "'a", "outlives": ["'static"]}},
    ]});
    let method = json!({"id": 4, "name": "m", "visibility": "public", "attrs": [],
        "inner": {"function": {"sig": {"inputs": sig_inputs, "output": {"resolved_path": path("Result", 28, &[t.clone(), u8_.clone()])},
                                       "is_c_variadic": false},
                               "generics": generics,
                               "header": {"is_const": true, "is_unsafe": true, "is_async": false, "abi": "Rust"},
                               "has_body": true}}});
    let send = json!({"path": "Send", "id": 25, "args": null});
    let mut index = json!({
        "0": {"id": 0, "name": "fix", "visibility": "public", "attrs": [],
              "inner": {"module": {"is_crate": true, "items": [1, 30, 40], "is_stripped": false}}},
        "1": {"id": 1, "name": "Gen", "visibility": "public", "attrs": [],
              "inner": {"struct": {"kind": {"tuple": [2, 6]},
                                   "generics": {"params": [{"name": "'a", "kind": {"lifetime": {"outlives": []}}},
                                                           {"name": "T", "kind": {"type": {"bounds": [], "default": null,
                                                                                           "is_synthetic": false}}}],
                                                "where_predicates": []},
                                   "impls": [3, 10, 11, 12, 13, 14]}}},
        "2": {"id": 2, "name": "0", "visibility": "public", "attrs": [], "inner": {"struct_field": t}},
        "6": {"id": 6, "name": "1", "visibility": "default", "attrs": [], "inner": {"struct_field": u8_}},
        "40": {"id": 40, "name": "E", "visibility": "public", "attrs": ["non_exhaustive"],
               "inner": {"enum": {"generics": no_generics, "variants": [41, 42, 43], "has_stripped_variants": false,
                                  "impls": []}}},
        "41": {"id": 41, "name": "A", "visibility": "default", "attrs": [],
               "inner": {"variant": {"kind": "plain", "discriminant": null}}},
        "42": {"id": 42, "name": "B", "visibility": "default", "attrs": [],
               "inner": {"variant": {"kind": {"tuple": [44]}, "discriminant": null}}},
        "43": {"id": 43, "name": "C", "visibility": "default", "attrs": ["non_exhaustive"],
               "inner": {"variant": {"kind": {"struct": {"fields": [45], "has_stripped_fields": false}},
                                     "discriminant": null}}},
        "44": {"id": 44, "name": "0", "visibility": "default", "attrs": [], "inner": {"struct_field": u8_}},
        "45": {"id": 45, "name": "x", "visibility": "default", "attrs": [], "inner": {"struct_field": u8_}},
        "3": {"id": 3, "name": null, "visibility": "default", "attrs": [],
              "inner": {"impl": {"is_unsafe": false, "generics": no_generics, "trait": null,
                                 "for": {"resolved_path": path("Gen", 1, std::slice::from_ref(&t))},
                                 "items": [4, 5], "is_negative": false, "is_synthetic": false, "blanket_impl": null}}},
        "4": method,
        "5": {"id": 5, "name": "MAX", "visibility": "public", "attrs": [],
              "inner": {"assoc_const": {"type": {"primitive": "usize"}, "value": "_"}}},
        "30": {"id": 30, "name": "Tr", "visibility": "public", "attrs": [],
               "inner": {"trait": {"items": [31], "generics": no_generics, "bounds": []}}},
        "31": {"id": 31, "name": "A", "visibility": "default", "attrs": [],
               "inner": {"assoc_type": {"generics": no_generics, "bounds": [bound(json!({"path": "Clone", "id": 20, "args": null}), "none")],
                                        "type": null}}},
    });
    let object = index.as_object_mut().unwrap();
    let where_send = json!({"params": [{"name": "T", "kind": {"type": {"bounds": [], "default": null, "is_synthetic": false}}}],
        "where_predicates": [{"bound_predicate": {"type": {"generic": "T"}, "bounds": [bound(send.clone(), "none")],
                                                  "generic_params": []}}]});
    for (key, item) in [
        impl_item(10, send, true, false, true, where_send),
        impl_item(
            11,
            json!({"path": "Sync", "id": 29, "args": null}),
            true,
            true,
            false,
            no_generics.clone(),
        ),
        impl_item(
            12,
            json!({"path": "Freeze", "id": 32, "args": null}),
            true,
            false,
            false,
            no_generics.clone(),
        ),
        impl_item(
            13,
            json!({"path": "StructuralPartialEq", "id": 33, "args": null}),
            false,
            false,
            false,
            no_generics.clone(),
        ),
        impl_item(
            14,
            json!({"path": "Debug", "id": 34, "args": null}),
            false,
            false,
            false,
            no_generics,
        ),
    ] {
        object.insert(key, item);
    }
    let core = |path: &[&str]| json!({"crate_id": 2, "path": path, "kind": "trait"});
    json!({
        "root": 0, "format_version": FORMAT_VERSION, "index": index,
        "paths": {
            "1": {"crate_id": 0, "path": ["fix", "Gen"], "kind": "struct"}, "40": {"crate_id": 0, "path": ["fix", "E"], "kind": "enum"},
            "20": core(&["core", "clone", "Clone"]), "22": core(&["core", "ops", "Fn"]),
            "23": core(&["core", "iter", "Iterator"]), "24": core(&["alloc", "boxed", "Box"]),
            "25": core(&["core", "marker", "Send"]), "26": core(&["core", "option", "Option"]),
            "27": core(&["core", "marker", "Sized"]), "28": core(&["core", "result", "Result"]),
            "29": core(&["core", "marker", "Sync"]), "34": core(&["core", "fmt", "Debug"]),
        },
        "external_crates": {"2": {"name": "core"}},
    })
}

/// Every type, bound, generic and impl form, as written; `Freeze` and `StructuralPartialEq` impls left out.
#[test]
fn every_form_of_a_signature_renders_as_rust_writes_it() {
    let crates = Crates::new(
        Crate::parse(&rich().to_string()).unwrap(),
        vec![Crate::parse(&other().to_string()).unwrap()],
    )
    .unwrap();
    let text = render(&crates).unwrap();
    let mut expected = [
        "impl !core::marker::Sync for fix::Gen<T>",
        "#[non_exhaustive] pub enum fix::E",
        "#[non_exhaustive] pub variant fix::E::C { .. }",
        "impl core::fmt::Debug for fix::Gen<T>",
        "pub const fix::Gen::MAX: usize",
        "pub const unsafe fn fix::Gen::m<'b: 'a, U: core::clone::Clone + ?core::marker::Sized + ~const core::clone::Clone, const N: usize>\
         (&'b mut self, a: &'a [T], b: [u8; N], c: (u8,), d: (u8, i8), e: *const u8, f: *mut u8, \
         g: impl core::ops::Fn(u8) -> u8, h: alloc::boxed::Box<dyn core::marker::Send + 'a>, \
         i: <T as core::iter::Iterator>::Item, j: fn(u8) -> u8, k: core::option::Option<_>, l: ()) \
         -> core::result::Result<T, u8> where T: core::iter::Iterator<Item = u8>, 'a: 'static",
        "pub struct fix::Gen<'a, T>(T, _)",
        "pub variant field fix::E::C::x: u8",
        "pub variant fix::E::A",
        "pub variant fix::E::B(u8)",
        "pub trait fix::Tr",
        "type fix::Tr::A: core::clone::Clone",
        "unsafe impl<T> core::marker::Send for fix::Gen<T> where T: core::marker::Send",
    ];
    // The listing is sorted by line.
    expected.sort_unstable();
    assert_eq!(text, format!("{}\n", expected.join("\n")), "{text}");
}

#[test]
fn the_command_takes_check_or_nothing() {
    assert!(!super::check_mode(&[]).unwrap());
    assert!(super::check_mode(&["--check".to_owned()]).unwrap());
    assert!(super::check_mode(&["--chek".to_owned()]).is_err());
    assert!(super::check_mode(&["--check".to_owned(), "x".to_owned()]).is_err());
}

/// The nightly the snapshot is built with is the dated one Miri and the fuzz targets use.
#[test]
fn the_snapshot_is_built_with_the_dated_nightly() {
    let root = crate::repo_root().unwrap();
    let nightly = super::nightly(&root).unwrap();
    let file = std::fs::read_to_string(root.join(".github/nightly-toolchain")).unwrap();
    assert_eq!(nightly, file.trim());
    assert!(nightly.starts_with("nightly-20"), "{nightly}");
}
