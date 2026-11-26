//! Renders the public surface of a crate from rustdoc's JSON (format [`FORMAT_VERSION`]) as sorted lines of text:
//! one line per item, field, variant, method, associated item and trait impl, each named by the path a user writes
//! (`persia::TextOptions::boost`). Items another crate defines and the root re-exports (`pub use persia_engine::...`)
//! are expanded from that crate's JSON, as if defined at the re-export.
//!
//! Left out on purpose, so the listing changes only when the surface does: blanket impls (`impl<T> From<T> for T`),
//! auto-trait impls other than `Send`, `Sync`, `Unpin`, `UnwindSafe` and `RefUnwindSafe`, the unstable
//! `StructuralPartialEq` marker, doc comments, spans and attributes other than `#[non_exhaustive]`. Items hidden with
//! `#[doc(hidden)]` are not in rustdoc's JSON.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde_json::Value as J;

/// The rustdoc JSON format this renderer reads: the one of the dated nightly in `.github/nightly-toolchain`.
pub const FORMAT_VERSION: u64 = 61;

/// The auto traits whose impls are listed: the ones a user relies on (a handle moved to a thread, kept across a
/// panic). `Freeze` and `UnsafeUnpin` are unstable or compiler detail.
const AUTO_TRAITS: [&str; 5] = ["Send", "Sync", "Unpin", "UnwindSafe", "RefUnwindSafe"];

/// One crate's rustdoc JSON.
pub struct Crate {
    json: J,
}

impl Crate {
    /// Parses `text`, rustdoc's JSON of one crate.
    ///
    /// # Errors
    /// Text that is not JSON, or of another format version than [`FORMAT_VERSION`].
    pub fn parse(text: &str) -> Result<Self> {
        let json: J = serde_json::from_str(text).context("rustdoc JSON")?;
        let version = json.get("format_version").and_then(J::as_u64);
        if version != Some(FORMAT_VERSION) {
            bail!(
                "rustdoc JSON format {version:?}, this renderer reads {FORMAT_VERSION}: update it with the nightly \
                 in .github/nightly-toolchain"
            );
        }
        Ok(Self { json })
    }

    /// The crate's own name: the first segment of its root module's path.
    fn name(&self) -> Option<&str> {
        let root = self.json.get("root")?;
        self.item(root)?.get("name")?.as_str()
    }

    fn item(&self, id: &J) -> Option<&J> {
        self.json.get("index")?.get(id_key(id)?)
    }

    /// The canonical path of `id` (`persia_engine::schema::field::TextOptions`), from the crate's `paths`.
    fn canonical(&self, id: &J) -> Option<String> {
        let summary = self.json.get("paths")?.get(id_key(id)?)?;
        let path: Vec<&str> = summary
            .get("path")?
            .as_array()?
            .iter()
            .filter_map(J::as_str)
            .collect();
        Some(path.join("::"))
    }

    /// The id of the item this crate defines at `canonical`.
    fn find(&self, canonical: &str) -> Option<J> {
        let paths = self.json.get("paths")?.as_object()?;
        paths.iter().find_map(|(key, summary)| {
            let own = summary.get("crate_id").and_then(J::as_u64) == Some(0);
            let path: Vec<&str> = summary
                .get("path")?
                .as_array()?
                .iter()
                .filter_map(J::as_str)
                .collect();
            (own && path.join("::") == canonical).then(|| J::String(key.clone()))
        })
    }
}

/// An id as the key of `index` and `paths`: ids are integers in the format, keys their decimal text.
fn id_key(id: &J) -> Option<String> {
    match id {
        J::Number(n) => Some(n.to_string()),
        J::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// The crates of a rendering: the root and those it re-exports from, by name (`persia_engine`).
pub struct Crates {
    root: String,
    by_name: BTreeMap<String, Crate>,
}

impl Crates {
    /// `root`'s surface, with `others` to expand its re-exports from.
    ///
    /// # Errors
    /// A crate without a name.
    pub fn new(root: Crate, others: Vec<Crate>) -> Result<Self> {
        let root_name = root
            .name()
            .context("the root crate has no name")?
            .to_owned();
        let mut by_name = BTreeMap::new();
        for c in others.into_iter().chain([root]) {
            let name = c.name().context("a crate has no name")?.to_owned();
            by_name.insert(name, c);
        }
        Ok(Self {
            root: root_name,
            by_name,
        })
    }
}

/// Where an item is: its crate, by name, and its id there.
#[derive(Clone)]
struct At<'a> {
    krate: &'a str,
    id: J,
}

/// The lines of `crates`' root surface, sorted, one per line with a final newline.
///
/// # Errors
/// JSON whose shape this renderer does not know.
pub fn render(crates: &Crates) -> Result<String> {
    let mut r = Renderer {
        crates,
        names: BTreeMap::new(),
        lines: BTreeSet::new(),
    };
    let root = crates.by_name.get(&crates.root).context("no root crate")?;
    let root_id = root.json.get("root").context("no root id")?.clone();
    // Two passes: first the path a user writes for each reachable item, so a signature names `persia::Document`
    // and not `persia_engine::document::Document`; then the lines.
    let at = At {
        krate: &crates.root,
        id: root_id,
    };
    r.module(&at, &crates.root, Pass::Names)?;
    r.module(&at, &crates.root, Pass::Lines)?;
    let mut out = String::new();
    for line in &r.lines {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Names,
    Lines,
}

struct Renderer<'a> {
    crates: &'a Crates,
    /// Canonical path → the path a user writes.
    names: BTreeMap<String, String>,
    lines: BTreeSet<String>,
}

impl<'a> Renderer<'a> {
    fn krate(&self, name: &str) -> Result<&'a Crate> {
        self.crates
            .by_name
            .get(name)
            .with_context(|| format!("no rustdoc JSON for crate {name}"))
    }

    fn item(&self, at: &At<'_>) -> Result<&'a J> {
        self.krate(at.krate)?
            .item(&at.id)
            .with_context(|| format!("no item {} in {}", at.id, at.krate))
    }

    fn module(&mut self, at: &At<'a>, path: &str, pass: Pass) -> Result<()> {
        let item = self.item(at)?;
        let items = item
            .pointer("/inner/module/items")
            .and_then(J::as_array)
            .context("a module without items")?;
        for id in items {
            let child = At {
                krate: at.krate,
                id: id.clone(),
            };
            let child_item = self.item(&child)?;
            if !is_public(child_item) {
                continue;
            }
            let inner = child_item.get("inner").context("an item without inner")?;
            if let Some(u) = inner.get("use") {
                self.reexport(at.krate, u, path, pass)?;
            } else {
                let name = child_item
                    .get("name")
                    .and_then(J::as_str)
                    .context("an item without a name")?;
                self.item_at(&child, &format!("{path}::{name}"), pass)?;
            }
        }
        Ok(())
    }

    /// A `pub use`: the item it names, defined in this crate or another one, at `path::name`.
    fn reexport(&mut self, krate: &'a str, u: &J, path: &str, pass: Pass) -> Result<()> {
        let name = u
            .get("name")
            .and_then(J::as_str)
            .context("a use without a name")?;
        let source = u.get("source").and_then(J::as_str).unwrap_or(name);
        let Some(id) = u.get("id").filter(|id| !id.is_null()) else {
            self.line(pass, format!("pub use {path}::{name} = {source}"));
            return Ok(());
        };
        if u.get("is_glob").and_then(J::as_bool) == Some(true) {
            bail!("glob re-export {source}: not supported");
        }
        let c = self.krate(krate)?;
        if c.item(id).is_some() {
            let at = At {
                krate,
                id: id.clone(),
            };
            return self.item_at(&at, &format!("{path}::{name}"), pass);
        }
        let canonical = c
            .canonical(id)
            .with_context(|| format!("re-export {source} has no path"))?;
        let other = canonical.split("::").next().unwrap_or_default();
        let Some((other_name, other_crate)) = self.crates.by_name.get_key_value(other) else {
            // A crate outside the rendering (std): named, not expanded.
            self.line(pass, format!("pub use {path}::{name} = {canonical}"));
            return Ok(());
        };
        let at = At {
            krate: other_name.as_str(),
            id: other_crate
                .find(&canonical)
                .with_context(|| format!("{canonical} is not an item of {other}'s rustdoc JSON"))?,
        };
        self.item_at(&at, &format!("{path}::{name}"), pass)
    }

    fn line(&mut self, pass: Pass, line: String) {
        if pass == Pass::Lines {
            self.lines.insert(line);
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per item kind
    fn item_at(&mut self, at: &At<'a>, path: &str, pass: Pass) -> Result<()> {
        let item = self.item(at)?;
        if pass == Pass::Names
            && let Some(canonical) = self.krate(at.krate)?.canonical(&at.id)
        {
            self.names
                .entry(canonical)
                .or_insert_with(|| path.to_owned());
        }
        let inner = item.get("inner").context("an item without inner")?;
        let (kind, body) = inner
            .as_object()
            .and_then(|o| o.iter().next())
            .context("an empty inner")?;
        let attrs = attrs(item);
        match kind.as_str() {
            "module" => {
                self.line(pass, format!("pub mod {path}"));
                self.module(at, path, pass)?;
            }
            "struct" => {
                let generics = self.generics(at.krate, body.get("generics"))?;
                let kind = body.get("kind").context("a struct without kind")?;
                if let Some(fields) = kind.get("tuple").and_then(J::as_array) {
                    let mut types = Vec::new();
                    for field in fields {
                        let f = match field {
                            J::Null => None,
                            id => Some(self.item(&At {
                                krate: at.krate,
                                id: id.clone(),
                            })?),
                        };
                        types.push(match f {
                            Some(f) if is_public(f) => {
                                self.ty(at.krate, f.pointer("/inner/struct_field"))?
                            }
                            _ => "_".to_owned(),
                        });
                    }
                    self.line(
                        pass,
                        format!("{attrs}pub struct {path}{generics}({})", types.join(", ")),
                    );
                } else {
                    self.line(pass, format!("{attrs}pub struct {path}{generics}"));
                    if let Some(fields) = kind.pointer("/plain/fields").and_then(J::as_array) {
                        self.fields(at.krate, fields, path, "field", pass)?;
                    }
                }
                self.impls(at, body, path, pass)?;
            }
            "enum" => {
                let generics = self.generics(at.krate, body.get("generics"))?;
                self.line(pass, format!("{attrs}pub enum {path}{generics}"));
                for variant in body
                    .get("variants")
                    .and_then(J::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.variant(at.krate, variant, path, pass)?;
                }
                self.impls(at, body, path, pass)?;
            }
            "union" => {
                self.line(pass, format!("{attrs}pub union {path}"));
                self.impls(at, body, path, pass)?;
            }
            "function" => {
                let sig = self.function(at.krate, body, path)?;
                self.line(pass, sig);
            }
            "trait" => {
                self.line(pass, format!("{attrs}pub trait {path}"));
                for id in body
                    .get("items")
                    .and_then(J::as_array)
                    .into_iter()
                    .flatten()
                {
                    let child = At {
                        krate: at.krate,
                        id: id.clone(),
                    };
                    let line = self.assoc(&child, path, true)?;
                    self.line(pass, line);
                }
            }
            "constant" => {
                let ty = self.ty(at.krate, body.get("type"))?;
                self.line(pass, format!("pub const {path}: {ty}"));
            }
            "static" => {
                let ty = self.ty(at.krate, body.get("type"))?;
                self.line(pass, format!("pub static {path}: {ty}"));
            }
            "type_alias" => {
                let generics = self.generics(at.krate, body.get("generics"))?;
                let ty = self.ty(at.krate, body.get("type"))?;
                self.line(pass, format!("pub type {path}{generics} = {ty}"));
            }
            "macro" | "proc_macro" => self.line(pass, format!("pub macro {path}")),
            other => bail!("item kind {other} at {path}: not supported"),
        }
        Ok(())
    }

    fn fields(
        &mut self,
        krate: &'a str,
        fields: &[J],
        path: &str,
        what: &str,
        pass: Pass,
    ) -> Result<()> {
        for id in fields {
            let f = self.item(&At {
                krate,
                id: id.clone(),
            })?;
            // A variant's fields have the enum's visibility; a struct's, their own.
            if what == "field" && !is_public(f) {
                continue;
            }
            let name = f
                .get("name")
                .and_then(J::as_str)
                .context("a field without a name")?;
            let ty = self.ty(krate, f.pointer("/inner/struct_field"))?;
            self.line(pass, format!("pub {what} {path}::{name}: {ty}"));
        }
        Ok(())
    }

    fn variant(&mut self, krate: &'a str, id: &J, path: &str, pass: Pass) -> Result<()> {
        let v = self.item(&At {
            krate,
            id: id.clone(),
        })?;
        let name = v
            .get("name")
            .and_then(J::as_str)
            .context("a variant without a name")?;
        let attrs = attrs(v);
        let kind = v
            .pointer("/inner/variant/kind")
            .context("a variant without kind")?;
        let vpath = format!("{path}::{name}");
        if let Some(fields) = kind.get("tuple").and_then(J::as_array) {
            let mut types = Vec::new();
            for field in fields {
                types.push(if field.is_null() {
                    "_".to_owned()
                } else {
                    let f = self.item(&At {
                        krate,
                        id: field.clone(),
                    })?;
                    self.ty(krate, f.pointer("/inner/struct_field"))?
                });
            }
            self.line(
                pass,
                format!("{attrs}pub variant {vpath}({})", types.join(", ")),
            );
        } else if let Some(fields) = kind.pointer("/struct/fields").and_then(J::as_array) {
            self.line(pass, format!("{attrs}pub variant {vpath} {{ .. }}"));
            self.fields(krate, fields, &vpath, "variant field", pass)?;
        } else {
            self.line(pass, format!("{attrs}pub variant {vpath}"));
        }
        Ok(())
    }

    fn impls(&mut self, at: &At<'a>, body: &J, path: &str, pass: Pass) -> Result<()> {
        if pass == Pass::Names {
            return Ok(());
        }
        for id in body
            .get("impls")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            let imp = self.item(&At {
                krate: at.krate,
                id: id.clone(),
            })?;
            let i = imp
                .pointer("/inner/impl")
                .context("an impl without inner")?;
            if !i.get("blanket_impl").is_none_or(J::is_null) {
                continue;
            }
            let trait_ = i.get("trait").filter(|t| !t.is_null());
            match trait_ {
                None => {
                    for item in i.get("items").and_then(J::as_array).into_iter().flatten() {
                        let child = At {
                            krate: at.krate,
                            id: item.clone(),
                        };
                        if is_public(self.item(&child)?) {
                            let line = self.assoc(&child, path, false)?;
                            self.line(pass, line);
                        }
                    }
                }
                Some(t) => {
                    let name = t.get("path").and_then(J::as_str).unwrap_or_default();
                    let last = name.rsplit("::").next().unwrap_or(name);
                    let synthetic = i.get("is_synthetic").and_then(J::as_bool) == Some(true);
                    if (synthetic && !AUTO_TRAITS.contains(&last)) || last == "StructuralPartialEq"
                    {
                        continue;
                    }
                    let negative = if i.get("is_negative").and_then(J::as_bool) == Some(true) {
                        "!"
                    } else {
                        ""
                    };
                    let unsafe_ = if i.get("is_unsafe").and_then(J::as_bool) == Some(true) {
                        "unsafe "
                    } else {
                        ""
                    };
                    let generics = self.generics(at.krate, i.get("generics"))?;
                    let trait_text = self.path(at.krate, t)?;
                    let for_ = self.ty(at.krate, i.get("for"))?;
                    let wheres = self.where_clause(at.krate, i.get("generics"))?;
                    self.line(
                        pass,
                        format!(
                            "{unsafe_}impl{generics} {negative}{trait_text} for {for_}{wheres}"
                        ),
                    );
                }
            }
        }
        Ok(())
    }

    /// An associated item of a type's inherent impl, or a trait's item.
    fn assoc(&mut self, at: &At<'a>, path: &str, in_trait: bool) -> Result<String> {
        let item = self.item(at)?;
        let name = item
            .get("name")
            .and_then(J::as_str)
            .context("an associated item without a name")?;
        let inner = item.get("inner").context("an item without inner")?;
        let item_path = format!("{path}::{name}");
        if let Some(f) = inner.get("function") {
            return self.function(at.krate, f, &item_path);
        }
        if let Some(c) = inner.get("assoc_const") {
            let ty = self.ty(at.krate, c.get("type"))?;
            return Ok(format!("pub const {item_path}: {ty}"));
        }
        if let Some(t) = inner.get("assoc_type") {
            let bounds = self.bounds(at.krate, t.get("bounds"))?;
            let value = match t.get("type").filter(|v| !v.is_null()) {
                Some(v) => format!(" = {}", self.ty(at.krate, Some(v))?),
                None => String::new(),
            };
            let colon = if bounds.is_empty() {
                String::new()
            } else {
                format!(": {bounds}")
            };
            let vis = if in_trait { "" } else { "pub " };
            return Ok(format!("{vis}type {item_path}{colon}{value}"));
        }
        bail!("associated item {item_path}: not supported")
    }

    fn function(&self, krate: &str, f: &J, path: &str) -> Result<String> {
        let header = f.get("header");
        let flag = |key: &str| header.and_then(|h| h.get(key)).and_then(J::as_bool) == Some(true);
        let mut quals = String::new();
        if flag("is_const") {
            quals.push_str("const ");
        }
        if flag("is_async") {
            quals.push_str("async ");
        }
        if flag("is_unsafe") {
            quals.push_str("unsafe ");
        }
        let generics = self.generics(krate, f.get("generics"))?;
        let sig = f.get("sig").context("a function without sig")?;
        let mut args = Vec::new();
        for input in sig
            .get("inputs")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            let name = input.get(0).and_then(J::as_str).unwrap_or("_");
            let ty = input.get(1);
            args.push(if name == "self" {
                self_arg(ty).map_or_else(
                    || Ok::<_, anyhow::Error>(format!("self: {}", self.ty(krate, ty)?)),
                    Ok,
                )?
            } else {
                format!("{name}: {}", self.ty(krate, ty)?)
            });
        }
        let output = match sig.get("output").filter(|o| !o.is_null()) {
            Some(o) => format!(" -> {}", self.ty(krate, Some(o))?),
            None => String::new(),
        };
        let wheres = self.where_clause(krate, f.get("generics"))?;
        Ok(format!(
            "pub {quals}fn {path}{generics}({}){output}{wheres}",
            args.join(", ")
        ))
    }

    /// `<'a, T: Bound, const N: usize>`, without the synthetic params of `impl Trait` arguments.
    fn generics(&self, krate: &str, g: Option<&J>) -> Result<String> {
        let mut params = Vec::new();
        for p in g
            .and_then(|g| g.get("params"))
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            let name = p.get("name").and_then(J::as_str).unwrap_or("_");
            let kind = p.get("kind").context("a generic param without kind")?;
            if let Some(lt) = kind.get("lifetime") {
                let outlives: Vec<&str> = lt
                    .get("outlives")
                    .and_then(J::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(J::as_str)
                    .collect();
                params.push(if outlives.is_empty() {
                    name.to_owned()
                } else {
                    format!("{name}: {}", outlives.join(" + "))
                });
            } else if let Some(t) = kind.get("type") {
                if t.get("is_synthetic").and_then(J::as_bool) == Some(true) {
                    continue;
                }
                let bounds = self.bounds(krate, t.get("bounds"))?;
                params.push(if bounds.is_empty() {
                    name.to_owned()
                } else {
                    format!("{name}: {bounds}")
                });
            } else if let Some(c) = kind.get("const") {
                params.push(format!("const {name}: {}", self.ty(krate, c.get("type"))?));
            }
        }
        Ok(if params.is_empty() {
            String::new()
        } else {
            format!("<{}>", params.join(", "))
        })
    }

    fn where_clause(&self, krate: &str, g: Option<&J>) -> Result<String> {
        let mut preds = Vec::new();
        for p in g
            .and_then(|g| g.get("where_predicates"))
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(b) = p.get("bound_predicate") {
                let ty = self.ty(krate, b.get("type"))?;
                preds.push(format!("{ty}: {}", self.bounds(krate, b.get("bounds"))?));
            } else if let Some(l) = p.get("lifetime_predicate") {
                let lt = l.get("lifetime").and_then(J::as_str).unwrap_or("'_");
                let outlives: Vec<&str> = l
                    .get("outlives")
                    .and_then(J::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(J::as_str)
                    .collect();
                preds.push(format!("{lt}: {}", outlives.join(" + ")));
            } else if let Some(e) = p.get("eq_predicate") {
                preds.push(format!(
                    "{} = {}",
                    self.ty(krate, e.get("lhs"))?,
                    self.term(krate, e.get("rhs"))?
                ));
            }
        }
        Ok(if preds.is_empty() {
            String::new()
        } else {
            format!(" where {}", preds.join(", "))
        })
    }

    fn bounds(&self, krate: &str, bounds: Option<&J>) -> Result<String> {
        let mut out = Vec::new();
        for b in bounds.and_then(J::as_array).into_iter().flatten() {
            if let Some(t) = b.get("trait_bound") {
                let modifier = match t.get("modifier").and_then(J::as_str) {
                    Some("maybe") => "?",
                    Some("maybe_const") => "~const ",
                    _ => "",
                };
                out.push(format!(
                    "{modifier}{}",
                    self.path(krate, t.get("trait").context("a bound without trait")?)?
                ));
            } else if let Some(o) = b.get("outlives").and_then(J::as_str) {
                out.push(o.to_owned());
            } else if b.get("use").is_some() {
                out.push("use<..>".to_owned());
            }
        }
        Ok(out.join(" + "))
    }

    /// A path to a type or trait, with its generic args: the path a user writes when the rendering reaches the item,
    /// else its canonical path, else the path as rustdoc wrote it.
    fn path(&self, krate: &str, p: &J) -> Result<String> {
        let written = p.get("path").and_then(J::as_str).unwrap_or_default();
        let c = self.krate(krate)?;
        let canonical = p.get("id").and_then(|id| c.canonical(id));
        let name = match canonical {
            Some(canonical) => self.names.get(&canonical).cloned().unwrap_or(canonical),
            None => written.to_owned(),
        };
        let args = match p.get("args").filter(|a| !a.is_null()) {
            Some(a) => self.args(krate, a)?,
            None => String::new(),
        };
        Ok(format!("{name}{args}"))
    }

    fn args(&self, krate: &str, a: &J) -> Result<String> {
        if let Some(ab) = a.get("angle_bracketed") {
            let mut parts = Vec::new();
            for arg in ab.get("args").and_then(J::as_array).into_iter().flatten() {
                if let Some(t) = arg.get("type") {
                    parts.push(self.ty(krate, Some(t))?);
                } else if let Some(l) = arg.get("lifetime").and_then(J::as_str) {
                    parts.push(l.to_owned());
                } else if let Some(c) = arg.get("const") {
                    parts.push(c.get("expr").and_then(J::as_str).unwrap_or("_").to_owned());
                } else {
                    parts.push("_".to_owned());
                }
            }
            for c in ab
                .get("constraints")
                .and_then(J::as_array)
                .into_iter()
                .flatten()
            {
                let name = c.get("name").and_then(J::as_str).unwrap_or("_");
                if let Some(eq) = c.pointer("/binding/equality") {
                    parts.push(format!("{name} = {}", self.term(krate, Some(eq))?));
                } else if let Some(b) = c.pointer("/binding/constraint") {
                    parts.push(format!("{name}: {}", self.bounds(krate, Some(b))?));
                }
            }
            return Ok(if parts.is_empty() {
                String::new()
            } else {
                format!("<{}>", parts.join(", "))
            });
        }
        if let Some(p) = a.get("parenthesized") {
            let mut inputs = Vec::new();
            for i in p.get("inputs").and_then(J::as_array).into_iter().flatten() {
                inputs.push(self.ty(krate, Some(i))?);
            }
            let output = match p.get("output").filter(|o| !o.is_null()) {
                Some(o) => format!(" -> {}", self.ty(krate, Some(o))?),
                None => String::new(),
            };
            return Ok(format!("({}){output}", inputs.join(", ")));
        }
        Ok(String::new())
    }

    fn term(&self, krate: &str, t: Option<&J>) -> Result<String> {
        let t = t.context("a missing term")?;
        if let Some(ty) = t.get("type") {
            return self.ty(krate, Some(ty));
        }
        if let Some(c) = t.get("constant") {
            return Ok(c.get("expr").and_then(J::as_str).unwrap_or("_").to_owned());
        }
        self.ty(krate, Some(t))
    }

    fn ty(&self, krate: &str, t: Option<&J>) -> Result<String> {
        let t = t.context("a missing type")?;
        if let Some(p) = t.get("resolved_path") {
            return self.path(krate, p);
        }
        if let Some(g) = t.get("generic").and_then(J::as_str) {
            return Ok(g.to_owned());
        }
        if let Some(p) = t.get("primitive").and_then(J::as_str) {
            return Ok(p.to_owned());
        }
        if let Some(r) = t.get("borrowed_ref") {
            let lt = r
                .get("lifetime")
                .and_then(J::as_str)
                .map(|l| format!("{l} "))
                .unwrap_or_default();
            let m = if r.get("is_mutable").and_then(J::as_bool) == Some(true) {
                "mut "
            } else {
                ""
            };
            return Ok(format!("&{lt}{m}{}", self.ty(krate, r.get("type"))?));
        }
        if let Some(r) = t.get("raw_pointer") {
            let m = if r.get("is_mutable").and_then(J::as_bool) == Some(true) {
                "mut"
            } else {
                "const"
            };
            return Ok(format!("*{m} {}", self.ty(krate, r.get("type"))?));
        }
        if let Some(s) = t.get("slice") {
            return Ok(format!("[{}]", self.ty(krate, Some(s))?));
        }
        if let Some(a) = t.get("array") {
            let len = a.get("len").and_then(J::as_str).unwrap_or("_");
            return Ok(format!("[{}; {len}]", self.ty(krate, a.get("type"))?));
        }
        if let Some(items) = t.get("tuple").and_then(J::as_array) {
            let mut parts = Vec::new();
            for i in items {
                parts.push(self.ty(krate, Some(i))?);
            }
            return Ok(if parts.len() == 1 {
                format!("({},)", parts.join(""))
            } else {
                format!("({})", parts.join(", "))
            });
        }
        if let Some(b) = t.get("impl_trait") {
            return Ok(format!("impl {}", self.bounds(krate, Some(b))?));
        }
        if let Some(d) = t.get("dyn_trait") {
            let mut parts = Vec::new();
            for tr in d.get("traits").and_then(J::as_array).into_iter().flatten() {
                parts
                    .push(self.path(krate, tr.get("trait").context("a dyn trait without trait")?)?);
            }
            if let Some(l) = d.get("lifetime").and_then(J::as_str) {
                parts.push(l.to_owned());
            }
            return Ok(format!("dyn {}", parts.join(" + ")));
        }
        if let Some(q) = t.get("qualified_path") {
            let name = q.get("name").and_then(J::as_str).unwrap_or("_");
            let self_ty = self.ty(krate, q.get("self_type"))?;
            return Ok(match q.get("trait").filter(|t| !t.is_null()) {
                Some(tr) => format!("<{self_ty} as {}>::{name}", self.path(krate, tr)?),
                None => format!("{self_ty}::{name}"),
            });
        }
        if let Some(f) = t.get("function_pointer") {
            let sig = f.get("sig").context("a function pointer without sig")?;
            let mut inputs = Vec::new();
            for i in sig
                .get("inputs")
                .and_then(J::as_array)
                .into_iter()
                .flatten()
            {
                inputs.push(self.ty(krate, i.get(1))?);
            }
            let output = match sig.get("output").filter(|o| !o.is_null()) {
                Some(o) => format!(" -> {}", self.ty(krate, Some(o))?),
                None => String::new(),
            };
            return Ok(format!("fn({}){output}", inputs.join(", ")));
        }
        if t.get("infer").is_some() || t.as_str() == Some("infer") {
            return Ok("_".to_owned());
        }
        bail!("type {t}: not supported")
    }
}

/// `self`, `&self`, `&mut self` (or with a lifetime) for a receiver of type `Self`; `None` for another receiver type.
fn self_arg(ty: Option<&J>) -> Option<String> {
    let ty = ty?;
    if ty.get("generic").and_then(J::as_str) == Some("Self") {
        return Some("self".to_owned());
    }
    let r = ty.get("borrowed_ref")?;
    if r.pointer("/type/generic").and_then(J::as_str) != Some("Self") {
        return None;
    }
    let lt = r
        .get("lifetime")
        .and_then(J::as_str)
        .map(|l| format!("{l} "))
        .unwrap_or_default();
    let m = if r.get("is_mutable").and_then(J::as_bool) == Some(true) {
        "mut "
    } else {
        ""
    };
    Some(format!("&{lt}{m}self"))
}

fn is_public(item: &J) -> bool {
    item.get("visibility").and_then(J::as_str) == Some("public")
}

/// `#[non_exhaustive] `, the one attribute that is part of the surface, or nothing.
fn attrs(item: &J) -> &'static str {
    let non_exhaustive = item
        .get("attrs")
        .and_then(J::as_array)
        .is_some_and(|a| a.iter().any(|x| x.as_str() == Some("non_exhaustive")));
    if non_exhaustive {
        "#[non_exhaustive] "
    } else {
        ""
    }
}
