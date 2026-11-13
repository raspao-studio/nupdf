// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Removing what a PDF can do besides show itself: JavaScript, actions that
//! launch programs, import or submit data, or open other files, XFA forms,
//! and, by policy, links to the web and embedded files (DECISIONS D48).
//! MuPDF has no such command; this is nupdf's own.
//!
//! Every object is walked, so an action is found wherever it hangs: /A,
//! /OpenAction, /AA triggers, /Next chains, outline items, the /Names
//! /JavaScript tree. What is removed is reported by object and key path,
//! and `scan` on the result finds nothing left.

use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, Object};

/// Dictionaries and arrays nested deeper than this are not looked into.
const MAX_DEPTH: usize = 64;

/// What to do with web links (URI actions).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UriPolicy {
    /// Leave them.
    #[default]
    Keep,
    /// Remove them all.
    Remove,
    /// Keep links to these hosts (and their subdomains), remove the rest.
    Allow(Vec<String>),
}

/// What `sanitize` removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub javascript: bool,
    /// Launch actions: running a program or opening a file.
    pub launch: bool,
    /// SubmitForm and ImportData: sending form data out, reading it in.
    pub data: bool,
    /// GoToR and GoToE: opening another PDF.
    pub remote: bool,
    pub xfa: bool,
    pub attachments: bool,
    pub uri: UriPolicy,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            javascript: true,
            launch: true,
            data: true,
            remote: true,
            xfa: true,
            attachments: false,
            uri: UriPolicy::Keep,
        }
    }
}

/// One thing found (and, by `sanitize`, removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The object it was in; 0 for the trailer.
    pub object: i32,
    /// The key path to it inside that object, as `/AA/O` or `/Names/JavaScript`.
    pub path: String,
    /// javascript, launch, submit, import, remote, uri, xfa or attachments.
    pub kind: &'static str,
    /// What it said: the script's first characters, the URI, the program.
    pub detail: String,
}

fn name_of(doc: &Document, obj: &Object, key: &[u8]) -> Option<Vec<u8>> {
    doc.resolve(&doc.dict_get(obj, key))
        .as_name()
        .map(<[u8]>::to_vec)
}

fn text(doc: &Document, obj: &Object) -> String {
    match doc.resolve(obj) {
        Object::String(s) => crate::text_string::text_string(&s),
        Object::Stream(s) => doc
            .stream_data(&s)
            .map(|d| crate::text_string::text_string(&d))
            .unwrap_or_default(),
        Object::Dict(_) => text(doc, &doc.dict_get(obj, b"F")),
        _ => String::new(),
    }
}

fn short(s: &str) -> String {
    let s: String = s.chars().take(80).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The host of a URI: after `scheme://`, up to the port, path or query.
fn host(uri: &str) -> Option<String> {
    let rest = uri.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    Some(host.to_ascii_lowercase())
}

impl Policy {
    /// Whether `action` (a resolved action dictionary) goes, and as what.
    fn removes(&self, doc: &Document, action: &Object) -> Option<(&'static str, String)> {
        let s = name_of(doc, action, b"S")?;
        let found = match s.as_slice() {
            b"JavaScript" if self.javascript => {
                ("javascript", text(doc, &doc.dict_get(action, b"JS")))
            }
            b"Rendition"
                if self.javascript && !matches!(doc.dict_get(action, b"JS"), Object::Null) =>
            {
                ("javascript", text(doc, &doc.dict_get(action, b"JS")))
            }
            b"Launch" if self.launch => {
                let target = ["F", "Win", "Unix", "Mac"]
                    .iter()
                    .map(|k| text(doc, &doc.dict_get(action, k.as_bytes())))
                    .find(|t| !t.is_empty())
                    .unwrap_or_default();
                ("launch", target)
            }
            b"SubmitForm" if self.data => ("submit", text(doc, &doc.dict_get(action, b"F"))),
            b"ImportData" if self.data => ("import", text(doc, &doc.dict_get(action, b"F"))),
            b"GoToR" | b"GoToE" if self.remote => {
                ("remote", text(doc, &doc.dict_get(action, b"F")))
            }
            b"URI" => {
                let uri = match doc.resolve(&doc.dict_get(action, b"URI")) {
                    Object::String(s) => String::from_utf8_lossy(&s).into_owned(),
                    _ => String::new(),
                };
                let goes = match &self.uri {
                    UriPolicy::Keep => false,
                    UriPolicy::Remove => true,
                    UriPolicy::Allow(hosts) => !host(&uri).is_some_and(|h| {
                        hosts.iter().any(|a| {
                            let a = a.to_ascii_lowercase();
                            h == a || h.ends_with(&format!(".{a}"))
                        })
                    }),
                };
                if !goes {
                    return None;
                }
                ("uri", uri)
            }
            _ => return None,
        };
        Some((found.0, short(&found.1)))
    }
}

/// Walks `obj`, returning it without what the policy removes, and what
/// that was. `obj` is the value as stored; references are not followed
/// (each object is walked on its own), except to look at an action a key
/// points to.
struct Walk<'a> {
    doc: &'a Document,
    policy: &'a Policy,
    object: i32,
    found: Vec<Finding>,
}

impl Walk<'_> {
    fn note(&mut self, path: &str, kind: &'static str, detail: String) {
        self.found.push(Finding {
            object: self.object,
            path: path.to_string(),
            kind,
            detail,
        });
    }

    /// Whether the value at `path` is an action the policy removes; it is
    /// noted when it is.
    fn action_goes(&mut self, path: &str, value: &Object) -> bool {
        let resolved = self.doc.resolve(value);
        if resolved.as_dict().is_none() {
            return false;
        }
        match self.policy.removes(self.doc, &resolved) {
            Some((kind, detail)) => {
                // An action of its own object is noted once, as that object.
                if !matches!(value, Object::Ref(_)) {
                    self.note(path, kind, detail);
                }
                true
            }
            None => false,
        }
    }

    /// An additional-actions dictionary: every trigger is an action, and
    /// those the policy removes go.
    fn triggers(&mut self, triggers: &Dict, path: &str, depth: usize) -> Dict {
        let mut kept = Dict::new();
        for (t, action) in triggers.iter() {
            let tp = format!("{path}/{}", String::from_utf8_lossy(t));
            if !self.action_goes(&tp, action) {
                kept.put(t, self.walk(action, &tp, depth + 1));
            }
        }
        kept
    }

    fn walk(&mut self, obj: &Object, path: &str, depth: usize) -> Object {
        if depth > MAX_DEPTH {
            return obj.clone();
        }
        match obj {
            Object::Dict(d) => Object::Dict(Arc::new(self.dict(d, path, depth))),
            Object::Array(a) => {
                let items: Vec<Object> = a
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| {
                        let p = format!("{path}[{i}]");
                        // An array of actions (/Next): drop the ones that go.
                        if path.ends_with("/Next") && self.action_goes(&p, v) {
                            return None;
                        }
                        Some(self.walk(v, &p, depth + 1))
                    })
                    .collect();
                Object::Array(Arc::new(items))
            }
            Object::Stream(s) => {
                let dict = self.dict(&s.dict, path, depth);
                if dict == s.dict {
                    obj.clone()
                } else {
                    let mut s = (**s).clone();
                    s.dict = dict;
                    Object::Stream(Arc::new(s))
                }
            }
            _ => obj.clone(),
        }
    }

    fn dict(&mut self, d: &Dict, path: &str, depth: usize) -> Dict {
        let doc = self.doc;
        let mut out = Dict::new();
        let is_catalog = matches!(d.get(b"Type"), Some(Object::Name(n)) if &**n == b"Catalog");
        for (key, value) in d.iter() {
            let p = format!("{path}/{}", String::from_utf8_lossy(key));
            match key {
                // Where actions hang.
                b"A" | b"OpenAction" | b"PA" | b"Next" if self.action_goes(&p, value) => continue,
                // Additional actions: every trigger is an action.
                b"AA" => {
                    if let Object::Dict(triggers) = value {
                        let kept = self.triggers(triggers, &p, depth);
                        if !kept.is_empty() {
                            out.put(key, Object::Dict(Arc::new(kept)));
                        }
                        continue;
                    }
                    // An /AA of its own object is walked as that object
                    // (`each_object` knows it for one); here only whether
                    // it is still there matters.
                    out.put(key, value.clone());
                    continue;
                }
                b"JavaScript" if self.policy.javascript && path.ends_with("/Names") => {
                    let count = name_tree_count(doc, value);
                    self.note(&p, "javascript", format!("{count} document scripts"));
                    continue;
                }
                b"EmbeddedFiles" if self.policy.attachments && path.ends_with("/Names") => {
                    let count = name_tree_count(doc, value);
                    self.note(&p, "attachments", format!("{count} embedded files"));
                    continue;
                }
                b"XFA" if self.policy.xfa && path.ends_with("/AcroForm") => {
                    self.note(&p, "xfa", "XFA form".into());
                    continue;
                }
                b"NeedsRendering" if self.policy.xfa && is_catalog => {
                    self.note(&p, "xfa", "NeedsRendering".into());
                    continue;
                }
                // Calculation order: scripts that run on every change.
                b"CO" if self.policy.javascript && path.ends_with("/AcroForm") => {
                    self.note(&p, "javascript", "calculation order".into());
                    continue;
                }
                b"FS"
                    if self.policy.attachments
                        && matches!(d.get(b"Subtype"), Some(Object::Name(n)) if &**n == b"FileAttachment") =>
                {
                    self.note(&p, "attachments", text(doc, value));
                    continue;
                }
                _ => {}
            }
            out.put(key, self.walk(value, &p, depth + 1));
        }
        out
    }
}

/// The number of leaves in a name tree.
fn name_tree_count(doc: &Document, node: &Object) -> usize {
    fn count(doc: &Document, node: &Object, depth: usize, seen: &mut Vec<i32>) -> usize {
        if depth > MAX_DEPTH {
            return 0;
        }
        if let Object::Ref(r) = node {
            if seen.contains(&r.num) {
                return 0;
            }
            seen.push(r.num);
        }
        let names = doc
            .resolve(&doc.dict_get(node, b"Names"))
            .as_array()
            .map_or(0, |a| a.len() / 2);
        let kids = doc.resolve(&doc.dict_get(node, b"Kids"));
        names
            + kids
                .as_array()
                .unwrap_or(&[])
                .iter()
                .map(|k| count(doc, k, depth + 1, seen))
                .sum::<usize>()
    }
    count(doc, node, 0, &mut Vec::new())
}

fn top_path(doc: &Document, num: i32) -> String {
    // The catalog's and the AcroForm's keys are named by where they are:
    // an /AcroForm or /Names of its own object is walked as what it is.
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let catalog = doc.resolve(&root);
    let held = |key: &[u8]| {
        catalog
            .as_dict()
            .and_then(|d| d.get(key))
            .and_then(Object::as_ref)
            .map(|r| r.num)
    };
    if held(b"AcroForm") == Some(num) {
        return "/AcroForm".into();
    }
    if held(b"Names") == Some(num) {
        return "/Names".into();
    }
    String::new()
}

/// The objects that are some dictionary's /AA, found in every object
/// down to the depth limit.
fn additional_actions(doc: &Document) -> std::collections::HashSet<i32> {
    fn find(obj: &Object, depth: usize, out: &mut std::collections::HashSet<i32>) {
        if depth > MAX_DEPTH {
            return;
        }
        let dict = match obj {
            Object::Dict(d) => d.as_ref(),
            Object::Stream(s) => &s.dict,
            Object::Array(a) => {
                for v in a.iter() {
                    find(v, depth + 1, out);
                }
                return;
            }
            _ => return,
        };
        for (key, value) in dict.iter() {
            match (key, value) {
                (b"AA", Object::Ref(r)) => {
                    out.insert(r.num);
                }
                _ => find(value, depth + 1, out),
            }
        }
    }
    let mut out = std::collections::HashSet::new();
    for num in 1..i32::try_from(doc.xref_len()).unwrap_or(i32::MAX) {
        if let Some(obj) = doc.object(num) {
            find(&obj, 0, &mut out);
        }
    }
    out
}

fn each_object(
    doc: &Document,
    policy: &Policy,
    mut f: impl FnMut(i32, &Object, Walk),
) -> Vec<Finding> {
    let mut found = Vec::new();
    let aa_objects = additional_actions(doc);
    for num in 1..i32::try_from(doc.xref_len()).unwrap_or(i32::MAX) {
        let Some(obj) = doc.object(num) else { continue };
        let mut w = Walk {
            doc,
            policy,
            object: num,
            found: Vec::new(),
        };
        // An action that is an object of its own goes: the keys pointing
        // to it are dropped, and it is emptied.
        if obj.as_dict().is_some()
            && let Some((kind, detail)) = policy.removes(doc, &obj)
        {
            found.push(Finding {
                object: num,
                path: String::new(),
                kind,
                detail,
            });
            f(num, &Object::Null, w);
            continue;
        }
        let new = match &obj {
            Object::Dict(d) if aa_objects.contains(&num) => {
                Object::Dict(Arc::new(w.triggers(d, "/AA", 0)))
            }
            _ => w.walk(&obj, &top_path(doc, num), 0),
        };
        found.extend(w.found.iter().cloned());
        if new != obj {
            f(num, &new, w);
        }
    }
    found
}

/// What `sanitize` would remove under `policy`, changing nothing.
pub fn scan(doc: &Document, policy: &Policy) -> Vec<Finding> {
    each_object(doc, policy, |_, _, _| {})
}

/// Removes from the document's overlay what `policy` removes, and says
/// what that was.
pub fn sanitize(doc: &Document, policy: &Policy) -> Vec<Finding> {
    let mut changes = Vec::new();
    let found = each_object(doc, policy, |num, new, _| changes.push((num, new.clone())));
    for (num, new) in changes {
        doc.alter_object(num, new);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_come_out_of_uris() {
        assert_eq!(
            host("https://user@Example.com:8080/x?y").as_deref(),
            Some("example.com")
        );
        assert_eq!(host("mailto:someone@example.com"), None);
        assert_eq!(
            host("http://a.b.example.org").as_deref(),
            Some("a.b.example.org")
        );
    }

    /// Something of each kind, in the places they hang: the catalog's
    /// /OpenAction and /NeedsRendering, an /AcroForm and /Names of their
    /// own, a /Next array, additional actions direct and of their own, a
    /// stream's /AA, a file attachment and nesting past the depth limit.
    fn active() -> Document {
        let mut deep = String::from("<</A<</S/Launch/F(deep.exe)>>>>");
        for _ in 0..70 {
            deep = format!("[{deep}]");
        }
        let objs = [
            "<</Type/Catalog/Pages 2 0 R/AcroForm 3 0 R/Names 4 0 R/NeedsRendering true\
             /OpenAction<</S/JavaScript/JS 5 0 R>>>>"
                .to_string(),
            "<</Type/Pages/Kids[]/Count 0>>".into(),
            "<</Fields[]/XFA 6 0 R/CO[]>>".into(),
            "<</JavaScript<</Kids[7 0 R 7 0 R]>>/EmbeddedFiles<</Names[(a)8 0 R(b)8 0 R]>>>>"
                .into(),
            String::from_utf8_lossy(&crate::testutil::stream("", b"app.alert(\n  'hi'  )")).into(),
            "[]".into(),
            "<</Names[(one)9 0 R]>>".into(),
            "<</Type/Filespec/F(a.txt)>>".into(),
            "<</S/JavaScript/JS(x)>>".into(),
            // 10: a link whose /Next chain mixes actions.
            "<</Subtype/Link/A<</S/GoTo/D[0/Fit]/Next[<</S/ImportData/F(in.fdf)>>\
             <</S/URI/URI(https://www.example.com/x)>><</S/URI/URI(https://evil.test/y)>>]>>>>"
                .into(),
            "<</Subtype/Widget/AA 12 0 R/PA<</S/Rendition/JS(play())>>>>".into(),
            "<</K<</S/SubmitForm/F(http://x/submit)>>/F<</S/Hide>>>>".into(),
            String::from_utf8_lossy(&crate::testutil::stream(
                "/AA<</O<</S/Launch/Win<</F(cmd.exe)>>>>>>",
                b"",
            ))
            .into(),
            "<</Subtype/FileAttachment/FS 8 0 R/AA<</PO<</S/GoToE/F(e.pdf)>>>>>>".into(),
            deep,
        ];
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        crate::testutil::doc_of(&refs)
    }

    #[test]
    fn everything_active_is_found_where_it_hangs() {
        let doc = active();
        let policy = Policy {
            attachments: true,
            uri: UriPolicy::Allow(vec!["Example.com".into()]),
            ..Policy::default()
        };
        let found: Vec<(i32, String, &str, String)> = scan(&doc, &policy)
            .into_iter()
            .map(|f| (f.object, f.path, f.kind, f.detail))
            .collect();
        let f = |o: i32, p: &str, k: &'static str, d: &str| (o, p.to_string(), k, d.to_string());
        assert_eq!(
            found,
            [
                f(1, "/NeedsRendering", "xfa", "NeedsRendering"),
                f(1, "/OpenAction", "javascript", "app.alert( 'hi' )"),
                f(3, "/AcroForm/XFA", "xfa", "XFA form"),
                f(3, "/AcroForm/CO", "javascript", "calculation order"),
                f(4, "/Names/JavaScript", "javascript", "1 document scripts"),
                f(4, "/Names/EmbeddedFiles", "attachments", "2 embedded files"),
                f(9, "", "javascript", "x"),
                f(10, "/A/Next[0]", "import", "in.fdf"),
                f(10, "/A/Next[2]", "uri", "https://evil.test/y"),
                f(11, "/PA", "javascript", "play()"),
                f(12, "/AA/K", "submit", "http://x/submit"),
                f(13, "/AA/O", "launch", "cmd.exe"),
                f(14, "/FS", "attachments", "a.txt"),
                f(14, "/AA/PO", "remote", "e.pdf"),
            ]
        );
        // What sanitize removes, scan no longer finds.
        assert_eq!(sanitize(&doc, &policy).len(), found.len());
        assert!(scan(&doc, &policy).is_empty());
        let print =
            |n| crate::print::to_string(&doc.object(n).unwrap_or(Object::Null), Default::default());
        assert_eq!(print(9), "null");
        assert_eq!(
            print(10),
            "<</Subtype/Link/A<</S/GoTo/D[0/Fit]/Next[<</S/URI/URI(https://www.example.com/x)>>]>>>>"
        );
        assert_eq!(print(12), "<</F<</S/Hide>>>>");
        // Links to the web go only when asked.
        let doc = active();
        let keep = scan(&doc, &Policy::default());
        assert!(
            keep.iter()
                .all(|f| f.kind != "uri" && f.kind != "attachments")
        );
        let all = scan(
            &doc,
            &Policy {
                uri: UriPolicy::Remove,
                ..Policy::default()
            },
        );
        assert_eq!(all.iter().filter(|f| f.kind == "uri").count(), 2);
    }
}
