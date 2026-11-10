//! A subset of the PDF Association's Arlington PDF Model, applied to the
//! dictionaries nupdf writes or edits: trailer or xref stream, catalog,
//! page tree and pages, link annotations with their actions, border styles
//! and file specifications, the outline, and the document information.
//!
//! The rules taken from each definition are the ones that need no
//! predicates: a key marked required must be there; a value must have one
//! of the listed types (`fn:` version wrappers are looked through); a name
//! from a fixed list must be on it; a key marked indirect must be a
//! reference, and one marked `fn:MustBeDirect()` must not be. Keys the
//! model does not list are not reported. The definitions are vendored in
//! `xtask/data/arlington`, unmodified, under the Apache License 2.0.

use std::collections::HashMap;

use nupdf::{Document, Object};

macro_rules! tsv {
    ($($name:literal),*) => {
        &[$(($name, include_str!(concat!("../data/arlington/", $name, ".tsv")))),*]
    };
}

const DEFINITIONS: &[(&str, &str)] = tsv!(
    "ActionGoTo",
    "ActionGoToR",
    "ActionLaunch",
    "ActionURI",
    "AnnotLink",
    "BorderStyle",
    "Catalog",
    "DocInfo",
    "FileSpecification",
    "FileTrailer",
    "Outline",
    "OutlineItem",
    "PageObject",
    "PageTreeNode",
    "PageTreeNodeRoot",
    "XRefStream"
);

/// Trees and chains deeper or longer than this are not followed.
const MAX_WALK: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
enum Indirect {
    Must,
    MustNot,
    Either,
}

/// One key of a definition: its types, each with a fixed list of values
/// when the model gives a plain one.
#[derive(Debug, Clone)]
struct Rule {
    key: String,
    types: Vec<(String, Option<Vec<String>>)>,
    required: bool,
    /// Found on an ancestor in the page tree when not on the node.
    inheritable: bool,
    indirect: Indirect,
}

/// The definitions, by name.
#[derive(Debug)]
pub struct Model {
    rules: HashMap<&'static str, Vec<Rule>>,
}

/// `fn:SinceVersion(1.5,stream)` and the like: the last plain argument.
fn unwrap_fn(s: &str) -> String {
    let mut s = s.trim();
    while let Some(rest) = s.strip_prefix("fn:") {
        let Some(open) = rest.find('(') else { break };
        let inner = rest[open + 1..].trim_end_matches(')');
        s = inner.rsplit(',').next().unwrap_or(inner).trim();
    }
    s.to_string()
}

/// A `[a,b]` list with no predicates in it, as plain values. `[]` allows
/// any value.
fn plain_list(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    if s.is_empty() || s.contains("fn:") {
        return None;
    }
    let inner = s.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return None;
    }
    Some(inner.split(',').map(|v| v.trim().to_string()).collect())
}

impl Model {
    pub fn load() -> Model {
        let mut rules = HashMap::new();
        for (name, text) in DEFINITIONS {
            let mut list = Vec::new();
            for line in text.lines().skip(1) {
                let cols: Vec<&str> = line.split('\t').collect();
                let col = |i: usize| cols.get(i).copied().unwrap_or("");
                let types: Vec<&str> = col(1).split(';').collect();
                let possible: Vec<&str> = col(8).split(';').collect();
                list.push(Rule {
                    key: col(0).to_string(),
                    types: types
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (unwrap_fn(t), possible.get(i).and_then(|p| plain_list(p))))
                        .collect(),
                    required: col(4) == "TRUE",
                    inheritable: col(6) == "TRUE",
                    indirect: match col(5) {
                        "TRUE" => Indirect::Must,
                        "fn:MustBeDirect()" => Indirect::MustNot,
                        _ => Indirect::Either,
                    },
                });
            }
            rules.insert(*name, list);
        }
        Model { rules }
    }

    /// Every violation in `doc`, as text naming the definition, the object
    /// and the key.
    pub fn validate(&self, doc: &Document) -> Vec<String> {
        let mut v = Walker {
            model: self,
            doc,
            out: Vec::new(),
            steps: 0,
            seen: Vec::new(),
        };
        let trailer = Object::Dict(std::sync::Arc::new(doc.trailer()));
        let is_xref =
            matches!(doc.dict_get(&trailer, b"Type"), Object::Name(n) if n.as_ref() == b"XRef");
        v.check(
            if is_xref { "XRefStream" } else { "FileTrailer" },
            &trailer,
            "trailer",
        );
        if let Some(root) = get(doc, &trailer, b"Root") {
            v.check("Catalog", &root, &label(&root));
            if let Some(pages) = get(doc, &root, b"Pages") {
                v.check("PageTreeNodeRoot", &pages, &label(&pages));
                v.walk_kids(&pages, 0);
            }
            if let Some(outlines) = get(doc, &root, b"Outlines") {
                v.check("Outline", &outlines, &label(&outlines));
                if let Some(first) = get(doc, &outlines, b"First") {
                    v.walk_outline(&first, 0);
                }
            }
        }
        if let Some(info) = get(doc, &trailer, b"Info") {
            v.check("DocInfo", &info, &label(&info));
        }
        v.out
    }
}

/// `cargo xtask arlington`: each file's violations, one JSON line each.
pub fn report(files: &[std::path::PathBuf]) -> anyhow::Result<()> {
    let model = Model::load();
    for file in files {
        let doc = Document::open(std::fs::read(file)?)
            .map_err(|e| anyhow::anyhow!("{}: {e:?}", file.display()))?;
        let line = serde_json::json!({
            "file": file.display().to_string(),
            "violations": model.validate(&doc),
        });
        println!("{line}");
    }
    Ok(())
}

fn label(obj: &Object) -> String {
    match obj {
        Object::Ref(r) => format!("{} {} R", r.num, r.generation),
        _ => "direct".into(),
    }
}

/// A key's value, or None where the value is null, directly or through a
/// reference to nothing, which ISO 32000 7.3.7 treats as absent.
fn get(doc: &Document, container: &Object, key: &[u8]) -> Option<Object> {
    match doc.resolve(container) {
        Object::Dict(d) => d.get(key).cloned(),
        Object::Stream(s) => s.dict.get(key).cloned(),
        _ => None,
    }
    .filter(|v| !is_null(doc, v))
}

fn is_null(doc: &Document, v: &Object) -> bool {
    matches!(doc.resolve(v), Object::Null)
}

fn name_of(doc: &Document, v: Option<Object>) -> Option<Vec<u8>> {
    match v.map(|v| doc.resolve(&v)) {
        Some(Object::Name(n)) => Some(n.to_vec()),
        _ => None,
    }
}

/// The Arlington type a value has, among those it could match.
fn matches_type(doc: &Document, value: &Object, ty: &str) -> bool {
    let v = doc.resolve(value);
    let numbers = |n: usize| matches!(&v, Object::Array(a) if a.len() == n && a.iter().all(|x| doc.resolve(x).is_number()));
    match ty {
        "array" => matches!(v, Object::Array(_)),
        "boolean" => matches!(v, Object::Bool(_)),
        "dictionary" | "name-tree" | "number-tree" => matches!(v, Object::Dict(_)),
        "stream" => matches!(v, Object::Stream(_)),
        "integer" | "bitmask" => matches!(v, Object::Int(_)),
        "number" => v.is_number(),
        "name" => matches!(v, Object::Name(_)),
        "null" => matches!(v, Object::Null),
        "date" | "string" | "string-ascii" | "string-byte" | "string-text" => {
            matches!(v, Object::String(_))
        }
        "rectangle" => numbers(4),
        "matrix" => numbers(6),
        _ => true,
    }
}

fn kind(v: &Object) -> &'static str {
    match v {
        Object::Null => "null",
        Object::Bool(_) => "boolean",
        Object::Int(_) => "integer",
        Object::Real(_) => "real",
        Object::Name(_) => "name",
        Object::String(_) => "string",
        Object::Array(_) => "array",
        Object::Dict(_) => "dictionary",
        Object::Ref(_) => "reference",
        Object::Stream(_) => "stream",
    }
}

struct Walker<'a> {
    model: &'a Model,
    doc: &'a Document,
    out: Vec<String>,
    steps: usize,
    seen: Vec<i32>,
}

impl Walker<'_> {
    /// Applies definition `def` to the dictionary `obj` leads to.
    fn check(&mut self, def: &str, obj: &Object, at: &str) {
        let Some(rules) = self.model.rules.get(def) else {
            return;
        };
        let dict = match self.doc.resolve(obj) {
            Object::Dict(d) => (*d).clone(),
            Object::Stream(s) => s.dict.clone(),
            other => {
                self.out
                    .push(format!("{def} {at}: is {}, not a dictionary", kind(&other)));
                return;
            }
        };
        for rule in rules {
            let key = rule.key.as_bytes();
            if key == b"*" {
                continue;
            }
            let inherited;
            let mut value = dict.get(key).filter(|v| !is_null(self.doc, v));
            if value.is_none() && rule.inheritable {
                inherited = self.doc.inherited(obj, key).ok().flatten();
                value = inherited.as_ref();
            }
            let Some(value) = value else {
                if rule.required {
                    self.out
                        .push(format!("{def} {at}: missing required /{}", rule.key));
                }
                continue;
            };
            match rule.indirect {
                Indirect::Must if !matches!(value, Object::Ref(_)) => self
                    .out
                    .push(format!("{def} {at}: /{} must be indirect", rule.key)),
                Indirect::MustNot if matches!(value, Object::Ref(_)) => self
                    .out
                    .push(format!("{def} {at}: /{} must be direct", rule.key)),
                _ => {}
            }
            let fitting: Vec<&(String, Option<Vec<String>>)> = rule
                .types
                .iter()
                .filter(|(t, _)| matches_type(self.doc, value, t))
                .collect();
            if fitting.is_empty() {
                let types: Vec<&str> = rule.types.iter().map(|(t, _)| t.as_str()).collect();
                self.out.push(format!(
                    "{def} {at}: /{} is {}, not {}",
                    rule.key,
                    kind(&self.doc.resolve(value)),
                    types.join(" or ")
                ));
                continue;
            }
            if let Object::Name(n) = self.doc.resolve(value)
                && let Some((_, Some(allowed))) = fitting.iter().find(|(t, _)| t == "name")
                && !allowed.iter().any(|a| a.as_bytes() == n.as_ref())
            {
                self.out.push(format!(
                    "{def} {at}: /{} is /{}, not one of {}",
                    rule.key,
                    String::from_utf8_lossy(&n),
                    allowed.join(", ")
                ));
            }
        }
    }

    /// A node met before (by number), or the walk has gone on too long.
    fn stop(&mut self, obj: &Object) -> bool {
        self.steps += 1;
        if self.steps > MAX_WALK {
            return true;
        }
        if let Object::Ref(r) = obj {
            if self.seen.contains(&r.num) {
                return true;
            }
            self.seen.push(r.num);
        }
        false
    }

    fn walk_kids(&mut self, node: &Object, depth: usize) {
        if depth > 64 {
            return;
        }
        let Some(kids) = get(self.doc, node, b"Kids") else {
            return;
        };
        let items = match self.doc.resolve(&kids) {
            Object::Array(a) => (*a).clone(),
            _ => return,
        };
        for kid in items {
            if self.stop(&kid) {
                continue;
            }
            match name_of(self.doc, get(self.doc, &kid, b"Type")).as_deref() {
                Some(b"Pages") => {
                    self.check("PageTreeNode", &kid, &label(&kid));
                    self.walk_kids(&kid, depth + 1);
                }
                _ => {
                    self.check("PageObject", &kid, &label(&kid));
                    self.walk_annots(&kid);
                }
            }
        }
    }

    fn walk_annots(&mut self, page: &Object) {
        let Some(annots) = get(self.doc, page, b"Annots") else {
            return;
        };
        let items = match self.doc.resolve(&annots) {
            Object::Array(a) => (*a).clone(),
            _ => return,
        };
        for annot in items {
            if name_of(self.doc, get(self.doc, &annot, b"Subtype")).as_deref() != Some(b"Link") {
                continue;
            }
            self.check("AnnotLink", &annot, &label(&annot));
            if let Some(bs) = get(self.doc, &annot, b"BS") {
                self.check("BorderStyle", &bs, &format!("{}/BS", label(&annot)));
            }
            if let Some(action) = get(self.doc, &annot, b"A") {
                self.check_action(&action, &format!("{}/A", label(&annot)));
            }
        }
    }

    fn check_action(&mut self, action: &Object, at: &str) {
        let def = match name_of(self.doc, get(self.doc, action, b"S")).as_deref() {
            Some(b"URI") => "ActionURI",
            Some(b"GoTo") => "ActionGoTo",
            Some(b"GoToR") => "ActionGoToR",
            Some(b"Launch") => "ActionLaunch",
            _ => return,
        };
        self.check(def, action, at);
        if let Some(fs) = get(self.doc, action, b"F")
            && matches!(self.doc.resolve(&fs), Object::Dict(_))
        {
            self.check("FileSpecification", &fs, &format!("{at}/F"));
        }
    }

    fn walk_outline(&mut self, first: &Object, depth: usize) {
        if depth > 64 {
            return;
        }
        let mut item = Some(first.clone());
        while let Some(it) = item {
            if self.stop(&it) {
                return;
            }
            self.check("OutlineItem", &it, &label(&it));
            if let Some(action) = get(self.doc, &it, b"A") {
                self.check_action(&action, &format!("{}/A", label(&it)));
            }
            if let Some(child) = get(self.doc, &it, b"First") {
                self.walk_outline(&child, depth + 1);
            }
            item = get(self.doc, &it, b"Next");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_parse_into_rules() {
        let m = Model::load();
        let catalog = &m.rules["Catalog"];
        let pages = catalog
            .iter()
            .find(|r| r.key == "Pages")
            .map(|r| (r.required, r.indirect.clone()));
        assert_eq!(pages, Some((true, Indirect::Must)));
        let ty = catalog
            .iter()
            .find(|r| r.key == "Type")
            .map(|r| r.types.clone());
        assert_eq!(
            ty,
            Some(vec![(
                "name".to_string(),
                Some(vec!["Catalog".to_string()])
            )])
        );
        assert_eq!(unwrap_fn("fn:SinceVersion(1.5,stream)"), "stream");
    }

    #[test]
    fn a_wrong_type_and_a_missing_key_are_reported() {
        let doc = Document::open(
            &b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R/PageMode/Sideways>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/Rotate/Ninety>>endobj\n\
trailer<</Root 1 0 R/Size 4>>\n"[..],
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let got = Model::load().validate(&doc);
        assert!(got.contains(&"Catalog 1 0 R: /PageMode is /Sideways, not one of UseNone, UseOutlines, UseThumbs, FullScreen, UseOC, UseAttachments".to_string()), "{got:?}");
        assert!(
            got.contains(&"PageObject 3 0 R: /Rotate is name, not integer".to_string()),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|v| v.starts_with("PageObject 3 0 R: missing required /")),
            "{got:?}"
        );
    }

    #[test]
    fn a_null_value_is_an_absent_key_and_an_empty_list_allows_any_name() {
        // ISO 32000 7.3.7: a key whose value is null, directly or through a
        // reference to nothing, is treated as absent.
        let doc = Document::open(
            &b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R/Outlines 9 0 R/Dests null\
>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/Resources<<>>/MediaBox[0 0 1 1]\
/Annots[<</Type/Annot/Subtype/Link/Rect[0 0 1 1]/A<</S/GoTo/D/chapter1>>>>]>>endobj\n\
trailer<</Root 1 0 R/Info 8 0 R/Size 4>>\n"[..],
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let got = Model::load().validate(&doc);
        assert_eq!(got, Vec::<String>::new());
    }
}
