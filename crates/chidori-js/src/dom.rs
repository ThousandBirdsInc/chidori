//! A deterministic, replayable virtual DOM for the pure-Rust engine.
//!
//! This is the "DOM behind the host boundary" design, built out as a serious
//! implementation rather than a sketch. The document is a Rust-side arena driven
//! through ordinary DOM-shaped JavaScript, and three journals fall out of it:
//!
//! * **Mutation journal** (output) — every structural / attribute / text change
//!   as an ordered, serializable [`Mutation`]. This *is* the render protocol.
//! * **Event journal** (input) — every delivered event as an [`EventRecord`].
//! * **Measurement journal** (captured input) — every layout/measurement read
//!   (`getBoundingClientRect`, `offsetWidth`, …) as a [`MeasureRecord`]: queried
//!   from a [`MeasurementProvider`] in record mode, served from the journal in
//!   replay mode. Layout is the one DOM read that is genuinely non-deterministic
//!   (it depends on a real renderer), so it is captured exactly the way the
//!   engine already captures `fetch`/`crypto`/timers.
//!
//! Events and measurements are the *only* sources of non-determinism, and both
//! live in the journal. Therefore the same program + the same
//! [`SessionJournal`] reproduces the mutation journal and rendered HTML
//! byte-for-byte — the property behind time-travel, replay-as-test, and
//! fork-and-edit-rerun.
//!
//! ## Lifetime / GC
//!
//! Node wrapper objects are cached on the node for stable JS identity
//! (`el.parentNode === container` holds). To avoid an `Rc` cycle between the
//! document arena and the native closures its wrappers hold, every closure holds
//! a [`std::rc::Weak`] back-reference; the embedder's [`DomHandle`] holds the one
//! strong [`std::rc::Rc`]. The VM/realm therefore never keeps the document
//! alive: when the handle drops, the arena and all wrappers drop deterministically
//! (no leak, no reliance on the cycle collector).

use crate::value::{JsObject, Property, PropertyKey, PropertyKind, Value};
use crate::vm::{ErrorKind, Vm};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

// ---------------------------------------------------------------------------
// Journals
// ---------------------------------------------------------------------------

/// A single change to the document. The ordered stream is the render protocol
/// and the deterministic output journal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Mutation {
    Create { id: usize, tag: String },
    CreateText { id: usize, data: String },
    SetAttribute { id: usize, name: String, value: String },
    RemoveAttribute { id: usize, name: String },
    SetText { id: usize, data: String },
    Append { parent: usize, child: usize },
    InsertBefore { parent: usize, child: usize, before: usize },
    Remove { parent: usize, child: usize },
}

/// One delivered UI event (the input journal).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventRecord {
    pub target: usize,
    #[serde(rename = "type")]
    pub ty: String,
    pub detail: serde_json::Value,
    #[serde(default = "default_true")]
    pub bubbles: bool,
    #[serde(default = "default_true")]
    pub cancelable: bool,
}

fn default_true() -> bool {
    true
}

/// One captured measurement read (the captured-input journal). Addressed by
/// `(node, kind, seq)` — deterministic because node ids are stable and `seq` is
/// a per-(node, kind) counter, mirroring the host-call journal's `(site, seq)`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MeasureRecord {
    pub node: usize,
    pub kind: String,
    pub seq: u32,
    pub value: serde_json::Value,
}

/// The complete non-deterministic input journal for a session: everything needed
/// to replay a recorded run into byte-identical output.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionJournal {
    pub events: Vec<EventRecord>,
    pub measurements: Vec<MeasureRecord>,
}

/// Record live and journal, or replay from a loaded journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Record,
    Replay,
}

// ---------------------------------------------------------------------------
// Measurement provider (the captured-read seam)
// ---------------------------------------------------------------------------

/// A read-only snapshot of a node handed to a [`MeasurementProvider`] so it can
/// compute layout without touching the arena (which is mutably borrowed).
pub struct MeasuredNode {
    pub id: usize,
    pub tag: String,
    pub text: String,
    pub attrs: Vec<(String, String)>,
    pub child_count: usize,
}

/// The renderer-side seam queried in record mode. A real implementation forwards
/// to a layout engine; it must be deterministic given the tree (that is what
/// makes the captured value safe to replay). Results are JSON: a number for
/// `offsetWidth`-style reads, an object for `getBoundingClientRect`.
pub trait MeasurementProvider {
    fn measure(&self, kind: &str, node: &MeasuredNode) -> serde_json::Value;
}

// ---------------------------------------------------------------------------
// Internal node model
// ---------------------------------------------------------------------------

struct Listener {
    ty: String,
    handler: Value,
    capture: bool,
    once: bool,
}

#[derive(Clone)]
enum NodeKind {
    Document,
    Element(String),
    Text,
}

struct NodeData {
    kind: NodeKind,
    parent: Option<usize>,
    children: Vec<usize>,
    attrs: Vec<(String, String)>,
    text: String,
    listeners: Vec<Listener>,
    wrapper: Option<Value>,
}

impl NodeData {
    fn new(kind: NodeKind) -> NodeData {
        NodeData {
            kind,
            parent: None,
            children: Vec::new(),
            attrs: Vec::new(),
            text: String::new(),
            listeners: Vec::new(),
            wrapper: None,
        }
    }
}

/// Mutable per-event state shared between the dispatch loop and the JS event
/// object's `preventDefault` / `stopPropagation` methods.
#[derive(Default)]
struct EventFlags {
    stop_propagation: bool,
    stop_immediate: bool,
    default_prevented: bool,
}

// ---------------------------------------------------------------------------
// The document arena
// ---------------------------------------------------------------------------

/// The virtual document. Nodes are addressed by their arena index (`NodeId`),
/// assigned by sequential allocation so ids are stable and replay-deterministic.
pub struct Dom {
    nodes: Vec<NodeData>,
    document: usize,
    html: usize,
    body: usize,
    mutations: Vec<Mutation>,
    events: Vec<EventRecord>,
    // Measurement record/replay.
    mode: Mode,
    provider: Option<Rc<dyn MeasurementProvider>>,
    measure_seq: HashMap<(usize, String), u32>,
    measurements: Vec<MeasureRecord>,
    replay_measure: HashMap<(usize, String, u32), serde_json::Value>,
}

impl Dom {
    fn new() -> Dom {
        let mut dom = Dom {
            nodes: Vec::new(),
            document: 0,
            html: 0,
            body: 0,
            mutations: Vec::new(),
            events: Vec::new(),
            mode: Mode::Record,
            provider: None,
            measure_seq: HashMap::new(),
            measurements: Vec::new(),
            replay_measure: HashMap::new(),
        };
        let document = dom.new_node(NodeKind::Document);
        let html = dom.new_node(NodeKind::Element("html".to_string()));
        let body = dom.new_node(NodeKind::Element("body".to_string()));
        dom.document = document;
        dom.html = html;
        dom.body = body;
        dom.nodes[html].parent = Some(document);
        dom.nodes[document].children.push(html);
        dom.nodes[body].parent = Some(html);
        dom.nodes[html].children.push(body);
        dom.mutations.push(Mutation::Create { id: html, tag: "html".to_string() });
        dom.mutations.push(Mutation::Create { id: body, tag: "body".to_string() });
        dom.mutations.push(Mutation::Append { parent: html, child: body });
        dom
    }

    fn new_node(&mut self, kind: NodeKind) -> usize {
        let id = self.nodes.len();
        self.nodes.push(NodeData::new(kind));
        id
    }

    fn create_element(&mut self, tag: &str) -> usize {
        let id = self.new_node(NodeKind::Element(tag.to_string()));
        self.mutations.push(Mutation::Create { id, tag: tag.to_string() });
        id
    }

    fn create_text(&mut self, data: &str) -> usize {
        let id = self.new_node(NodeKind::Text);
        self.nodes[id].text = data.to_string();
        self.mutations.push(Mutation::CreateText { id, data: data.to_string() });
        id
    }

    fn is_ancestor(&self, maybe_ancestor: usize, node: usize) -> bool {
        let mut cur = Some(node);
        while let Some(id) = cur {
            if id == maybe_ancestor {
                return true;
            }
            cur = self.nodes[id].parent;
        }
        false
    }

    fn detach(&mut self, child: usize) {
        if let Some(parent) = self.nodes[child].parent {
            self.nodes[parent].children.retain(|&c| c != child);
            self.nodes[child].parent = None;
            self.mutations.push(Mutation::Remove { parent, child });
        }
    }

    /// Returns Err if the append would create a cycle (DOM `HierarchyRequestError`).
    fn append_child(&mut self, parent: usize, child: usize) -> Result<(), String> {
        if self.is_ancestor(child, parent) {
            return Err("append would create a cycle".to_string());
        }
        self.detach(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
        self.mutations.push(Mutation::Append { parent, child });
        Ok(())
    }

    fn insert_before(&mut self, parent: usize, child: usize, before: usize) -> Result<(), String> {
        if self.is_ancestor(child, parent) {
            return Err("insertBefore would create a cycle".to_string());
        }
        if self.nodes[before].parent != Some(parent) {
            return Err("insertBefore: reference node is not a child".to_string());
        }
        self.detach(child);
        let idx = self.nodes[parent]
            .children
            .iter()
            .position(|&c| c == before)
            .expect("reference child present");
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.insert(idx, child);
        self.mutations.push(Mutation::InsertBefore { parent, child, before });
        Ok(())
    }

    fn set_attribute(&mut self, id: usize, name: &str, value: &str) {
        if let Some(slot) = self.nodes[id].attrs.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value.to_string();
        } else {
            self.nodes[id].attrs.push((name.to_string(), value.to_string()));
        }
        self.mutations
            .push(Mutation::SetAttribute { id, name: name.to_string(), value: value.to_string() });
    }

    fn get_attribute(&self, id: usize, name: &str) -> Option<String> {
        self.nodes[id].attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    fn remove_attribute(&mut self, id: usize, name: &str) {
        let before = self.nodes[id].attrs.len();
        self.nodes[id].attrs.retain(|(n, _)| n != name);
        if self.nodes[id].attrs.len() != before {
            self.mutations.push(Mutation::RemoveAttribute { id, name: name.to_string() });
        }
    }

    fn set_text_content(&mut self, id: usize, data: &str) {
        if matches!(self.nodes[id].kind, NodeKind::Text) {
            self.nodes[id].text = data.to_string();
            self.mutations.push(Mutation::SetText { id, data: data.to_string() });
            return;
        }
        let kids = self.nodes[id].children.clone();
        for k in kids {
            self.detach(k);
        }
        if !data.is_empty() {
            let t = self.create_text(data);
            let _ = self.append_child(id, t);
        }
    }

    /// `innerHTML` setter: replace all children with the result of parsing
    /// `html`. Not a full HTML5 parser — it handles the well-formed, explicit
    /// markup that server-side renderers (e.g. `react-dom/server`) emit: tags,
    /// attributes, text, void/self-closing elements, comments, and entities.
    fn set_inner_html(&mut self, id: usize, html: &str) {
        let kids = self.nodes[id].children.clone();
        for k in kids {
            self.detach(k);
        }
        self.parse_into(id, html);
    }

    fn parse_into(&mut self, parent: usize, html: &str) {
        const VOID: &[&str] = &[
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link",
            "meta", "param", "source", "track", "wbr",
        ];
        let s: Vec<char> = html.chars().collect();
        let n = s.len();
        let mut i = 0;
        let mut stack = vec![parent];
        while i < n {
            if s[i] == '<' {
                // Comment / doctype.
                if i + 1 < n && s[i + 1] == '!' {
                    if i + 3 < n && s[i + 2] == '-' && s[i + 3] == '-' {
                        i += 4;
                        while i + 2 < n && !(s[i] == '-' && s[i + 1] == '-' && s[i + 2] == '>') {
                            i += 1;
                        }
                        i = (i + 3).min(n);
                    } else {
                        while i < n && s[i] != '>' {
                            i += 1;
                        }
                        i += 1;
                    }
                    continue;
                }
                // Close tag.
                if i + 1 < n && s[i + 1] == '/' {
                    i += 2;
                    let mut name = String::new();
                    while i < n && s[i] != '>' {
                        name.push(s[i]);
                        i += 1;
                    }
                    i += 1;
                    let name = name.trim().to_lowercase();
                    if let Some(pos) = stack.iter().rposition(|&nid| {
                        matches!(&self.nodes[nid].kind, NodeKind::Element(t) if *t == name)
                    }) {
                        if pos > 0 {
                            stack.truncate(pos);
                        }
                    }
                    continue;
                }
                // Open tag.
                i += 1;
                let mut name = String::new();
                while i < n && !s[i].is_whitespace() && s[i] != '>' && s[i] != '/' {
                    name.push(s[i]);
                    i += 1;
                }
                let name = name.to_lowercase();
                let mut attrs: Vec<(String, String)> = Vec::new();
                let mut self_close = false;
                loop {
                    while i < n && s[i].is_whitespace() {
                        i += 1;
                    }
                    if i >= n || s[i] == '>' {
                        i += 1;
                        break;
                    }
                    if s[i] == '/' {
                        self_close = true;
                        i += 1;
                        continue;
                    }
                    let mut an = String::new();
                    while i < n && !s[i].is_whitespace() && s[i] != '=' && s[i] != '>' && s[i] != '/'
                    {
                        an.push(s[i]);
                        i += 1;
                    }
                    while i < n && s[i].is_whitespace() {
                        i += 1;
                    }
                    let mut av = String::new();
                    if i < n && s[i] == '=' {
                        i += 1;
                        while i < n && s[i].is_whitespace() {
                            i += 1;
                        }
                        if i < n && (s[i] == '"' || s[i] == '\'') {
                            let q = s[i];
                            i += 1;
                            let mut raw = String::new();
                            while i < n && s[i] != q {
                                raw.push(s[i]);
                                i += 1;
                            }
                            i += 1;
                            av = decode_entities(&raw);
                        } else {
                            let mut raw = String::new();
                            while i < n && !s[i].is_whitespace() && s[i] != '>' {
                                raw.push(s[i]);
                                i += 1;
                            }
                            av = decode_entities(&raw);
                        }
                    }
                    if !an.is_empty() {
                        attrs.push((an, av));
                    }
                }
                let el = self.create_element(&name);
                for (k, v) in attrs {
                    self.set_attribute(el, &k, &v);
                }
                let top = *stack.last().unwrap();
                let _ = self.append_child(top, el);
                if !self_close && !VOID.contains(&name.as_str()) {
                    stack.push(el);
                }
            } else {
                let mut txt = String::new();
                while i < n && s[i] != '<' {
                    txt.push(s[i]);
                    i += 1;
                }
                let decoded = decode_entities(&txt);
                if !decoded.trim().is_empty() {
                    let top = *stack.last().unwrap();
                    let t = self.create_text(&decoded);
                    let _ = self.append_child(top, t);
                }
            }
        }
    }

    fn text_content(&self, id: usize) -> String {
        match &self.nodes[id].kind {
            NodeKind::Text => self.nodes[id].text.clone(),
            _ => {
                let mut out = String::new();
                for &c in &self.nodes[id].children {
                    out.push_str(&self.text_content(c));
                }
                out
            }
        }
    }

    /// Deep or shallow clone (listeners are never copied, per spec).
    fn clone_node(&mut self, id: usize, deep: bool) -> usize {
        let (kind, attrs, text) = {
            let n = &self.nodes[id];
            (n.kind.clone(), n.attrs.clone(), n.text.clone())
        };
        let new_id = match &kind {
            NodeKind::Element(tag) => self.create_element(tag),
            NodeKind::Text => self.create_text(&text),
            NodeKind::Document => self.new_node(NodeKind::Document),
        };
        for (k, v) in attrs {
            self.set_attribute(new_id, &k, &v);
        }
        if deep {
            let kids = self.nodes[id].children.clone();
            for k in kids {
                let ck = self.clone_node(k, true);
                let _ = self.append_child(new_id, ck);
            }
        }
        new_id
    }

    fn classes(&self, id: usize) -> Vec<String> {
        self.get_attribute(id, "class")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    fn set_classes(&mut self, id: usize, classes: &[String]) {
        if classes.is_empty() {
            self.remove_attribute(id, "class");
        } else {
            self.set_attribute(id, "class", &classes.join(" "));
        }
    }

    fn measured_node(&self, id: usize) -> MeasuredNode {
        let tag = match &self.nodes[id].kind {
            NodeKind::Element(t) => t.clone(),
            NodeKind::Text => "#text".to_string(),
            NodeKind::Document => "#document".to_string(),
        };
        MeasuredNode {
            id,
            tag,
            text: self.text_content(id),
            attrs: self.nodes[id].attrs.clone(),
            child_count: self.nodes[id].children.len(),
        }
    }

    /// Perform a captured measurement read: query the provider and journal it
    /// (record), or serve the journaled value (replay).
    fn measure(&mut self, id: usize, kind: &str) -> serde_json::Value {
        let key = (id, kind.to_string());
        let seq = {
            let c = self.measure_seq.entry(key).or_insert(0);
            let s = *c;
            *c += 1;
            s
        };
        match self.mode {
            Mode::Record => {
                let snap = self.measured_node(id);
                let value = self
                    .provider
                    .as_ref()
                    .map(|p| p.measure(kind, &snap))
                    .unwrap_or(serde_json::Value::Null);
                self.measurements.push(MeasureRecord {
                    node: id,
                    kind: kind.to_string(),
                    seq,
                    value: value.clone(),
                });
                value
            }
            Mode::Replay => self
                .replay_measure
                .get(&(id, kind.to_string(), seq))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }
    }

    fn render_html(&self, id: usize) -> String {
        match &self.nodes[id].kind {
            NodeKind::Text => escape_text(&self.nodes[id].text),
            NodeKind::Document => self.render_children(id),
            NodeKind::Element(tag) => {
                let mut s = format!("<{tag}");
                for (k, v) in &self.nodes[id].attrs {
                    s.push_str(&format!(" {k}=\"{}\"", escape_attr(v)));
                }
                s.push('>');
                s.push_str(&self.render_children(id));
                s.push_str(&format!("</{tag}>"));
                s
            }
        }
    }

    fn render_children(&self, id: usize) -> String {
        let mut s = String::new();
        for &c in &self.nodes[id].children {
            s.push_str(&self.render_html(c));
        }
        s
    }

    fn find_by_id(&self, dom_id: &str) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .find_map(|(i, n)| n.attrs.iter().any(|(k, v)| k == "id" && v == dom_id).then_some(i))
    }

    /// Collect descendants of `root` matching a simple selector, in document order.
    fn select(&self, root: usize, sel: &Selector, first_only: bool) -> Vec<usize> {
        let mut out = Vec::new();
        self.select_into(root, sel, first_only, &mut out);
        out
    }

    fn select_into(&self, node: usize, sel: &Selector, first_only: bool, out: &mut Vec<usize>) {
        for &c in &self.nodes[node].children {
            if self.matches(c, sel) {
                out.push(c);
                if first_only {
                    return;
                }
            }
            self.select_into(c, sel, first_only, out);
            if first_only && !out.is_empty() {
                return;
            }
        }
    }

    fn matches(&self, id: usize, sel: &Selector) -> bool {
        let tag = match &self.nodes[id].kind {
            NodeKind::Element(t) => t.as_str(),
            _ => return false,
        };
        if let Some(t) = &sel.tag {
            if !tag.eq_ignore_ascii_case(t) {
                return false;
            }
        }
        if let Some(want) = &sel.id {
            if self.get_attribute(id, "id").as_deref() != Some(want.as_str()) {
                return false;
            }
        }
        if !sel.classes.is_empty() {
            let have = self.classes(id);
            if !sel.classes.iter().all(|c| have.contains(c)) {
                return false;
            }
        }
        true
    }
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// Decode the HTML entities that server renderers emit (the five XML entities,
/// `&apos;`/`&nbsp;`, and numeric `&#NN;` / `&#xHH;`). Unknown entities pass
/// through verbatim.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            if let Some(semi) = chars[i + 1..].iter().position(|&c| c == ';') {
                let ent: String = chars[i + 1..i + 1 + semi].iter().collect();
                let decoded = match ent.as_str() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{00a0}'),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                        u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                    }
                    _ if ent.starts_with('#') => {
                        ent[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                if let Some(c) = decoded {
                    out.push(c);
                    i += semi + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// A single compound selector: optional tag, optional `#id`, zero or more
/// `.class`. Descendant combinator only (whitespace is not parsed as a
/// combinator here — one compound term).
struct Selector {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

fn parse_selector(s: &str) -> Selector {
    let s = s.trim();
    let mut tag = None;
    let mut id = None;
    let mut classes = Vec::new();
    let mut chars = s.chars().peekable();
    // leading tag (until a '.' or '#')
    let mut lead = String::new();
    while let Some(&c) = chars.peek() {
        if c == '.' || c == '#' {
            break;
        }
        lead.push(c);
        chars.next();
    }
    if !lead.is_empty() && lead != "*" {
        tag = Some(lead);
    }
    while let Some(c) = chars.next() {
        let mut tok = String::new();
        while let Some(&n) = chars.peek() {
            if n == '.' || n == '#' {
                break;
            }
            tok.push(n);
            chars.next();
        }
        match c {
            '#' => id = Some(tok),
            '.' => classes.push(tok),
            _ => {}
        }
    }
    Selector { tag, id, classes }
}

// ---------------------------------------------------------------------------
// Embedder handle
// ---------------------------------------------------------------------------

/// A cloneable, strong handle to the live document — the seam the host loop
/// drives. Holding this keeps the arena alive; dropping every clone frees it.
#[derive(Clone)]
pub struct DomHandle(Rc<RefCell<Dom>>);

impl DomHandle {
    pub fn body_id(&self) -> usize {
        self.0.borrow().body
    }

    /// Number of strong references to the document arena. At rest this is the
    /// count of live [`DomHandle`] clones — the wrapper closures hold only
    /// `Weak`s, so the VM/realm never contributes. A value of 1 after a run
    /// proves the arena is not leaked into a reference cycle.
    #[doc(hidden)]
    pub fn strong_count(&self) -> usize {
        Rc::strong_count(&self.0)
    }

    pub fn element_by_id(&self, dom_id: &str) -> Option<usize> {
        self.0.borrow().find_by_id(dom_id)
    }

    pub fn mutations(&self) -> Vec<Mutation> {
        self.0.borrow().mutations.clone()
    }

    pub fn drain_mutations(&self) -> Vec<Mutation> {
        std::mem::take(&mut self.0.borrow_mut().mutations)
    }

    pub fn events(&self) -> Vec<EventRecord> {
        self.0.borrow().events.clone()
    }

    /// The full non-deterministic input journal (events + captured measurements).
    pub fn journal(&self) -> SessionJournal {
        let dom = self.0.borrow();
        SessionJournal {
            events: dom.events.clone(),
            measurements: dom.measurements.clone(),
        }
    }

    pub fn render_html(&self) -> String {
        let dom = self.0.borrow();
        dom.render_html(dom.html)
    }

    /// Install the renderer-side measurement provider (record mode).
    pub fn set_measurement_provider(&self, provider: Rc<dyn MeasurementProvider>) {
        self.0.borrow_mut().provider = Some(provider);
    }

    pub fn mode(&self) -> Mode {
        self.0.borrow().mode
    }

    /// Switch this document into replay mode and load the recorded journal:
    /// measurement reads are served from it, and [`DomHandle::replay`] drives the
    /// recorded events. The document must be freshly built by the same program.
    pub fn load_journal_for_replay(&self, journal: &SessionJournal) {
        let mut dom = self.0.borrow_mut();
        dom.mode = Mode::Replay;
        dom.provider = None;
        dom.replay_measure = journal
            .measurements
            .iter()
            .map(|m| ((m.node, m.kind.clone(), m.seq), m.value.clone()))
            .collect();
    }

    /// Deliver an event with full W3C-style capture/target/bubble dispatch.
    /// Returns `false` if a handler called `preventDefault()` (i.e. the default
    /// action is cancelled), matching `EventTarget.dispatchEvent`.
    pub fn dispatch_event(
        &self,
        vm: &mut Vm,
        target: usize,
        ty: &str,
        detail: serde_json::Value,
    ) -> Result<bool, String> {
        self.dispatch_event_opts(vm, target, ty, detail, true, true)
    }

    pub fn dispatch_event_opts(
        &self,
        vm: &mut Vm,
        target: usize,
        ty: &str,
        detail: serde_json::Value,
        bubbles: bool,
        cancelable: bool,
    ) -> Result<bool, String> {
        {
            let mut dom = self.0.borrow_mut();
            if target >= dom.nodes.len() {
                return Err(format!("dispatch_event: no node #{target}"));
            }
            if dom.mode == Mode::Record {
                dom.events.push(EventRecord {
                    target,
                    ty: ty.to_string(),
                    detail: detail.clone(),
                    bubbles,
                    cancelable,
                });
            }
        }

        // Propagation path: target -> ... -> root.
        let path: Vec<usize> = {
            let dom = self.0.borrow();
            let mut p = Vec::new();
            let mut cur = Some(target);
            while let Some(id) = cur {
                p.push(id);
                cur = dom.nodes[id].parent;
            }
            p
        };

        // Build the ordered (node, phase) plan. phase: 1=capture, 2=target, 3=bubble.
        let mut plan: Vec<(usize, u8)> = Vec::new();
        for &anc in path[1..].iter().rev() {
            plan.push((anc, 1));
        }
        plan.push((target, 2));
        if bubbles {
            for &anc in path[1..].iter() {
                plan.push((anc, 3));
            }
        }

        let flags = Rc::new(RefCell::new(EventFlags::default()));
        let weak = Rc::downgrade(&self.0);
        let event = make_event(vm, &weak, target, ty, &detail, bubbles, cancelable, &flags);

        'outer: for (node, phase) in plan {
            let ct = node_wrapper(vm, &self.0, node);
            if let Value::Object(eo) = &event {
                vm.define_value(eo, "currentTarget", ct.clone());
                vm.define_value(eo, "eventPhase", Value::Number(phase as f64));
            }
            // Snapshot the listeners that apply in this phase, in registration order.
            let snapshot: Vec<(Value, bool, bool)> = {
                let dom = self.0.borrow();
                dom.nodes[node]
                    .listeners
                    .iter()
                    .filter(|l| {
                        l.ty == ty
                            && match phase {
                                1 => l.capture,
                                3 => !l.capture,
                                _ => true, // AT_TARGET: both
                            }
                    })
                    .map(|l| (l.handler.clone(), l.once, l.capture))
                    .collect()
            };
            for (handler, once, capture) in snapshot {
                if once {
                    remove_listener(&self.0, node, ty, &handler, capture);
                }
                if let Err(e) = vm.call(handler, ct.clone(), &[event.clone()]) {
                    let msg = vm.error_to_string(&e);
                    return Err(format!("event handler threw: {msg}"));
                }
                if flags.borrow().stop_immediate {
                    break 'outer;
                }
            }
            if flags.borrow().stop_propagation {
                break 'outer;
            }
        }

        let _ = vm.run_jobs_until_blocked();
        let prevented = flags.borrow().default_prevented;
        Ok(!prevented)
    }

    /// Replay a recorded event journal against this (fresh, replay-mode) document.
    pub fn replay(&self, vm: &mut Vm, events: &[EventRecord]) -> Result<(), String> {
        for ev in events {
            self.dispatch_event_opts(
                vm,
                ev.target,
                &ev.ty,
                ev.detail.clone(),
                ev.bubbles,
                ev.cancelable,
            )?;
        }
        Ok(())
    }

    /// Back-compat alias.
    pub fn replay_events(&self, vm: &mut Vm, events: &[EventRecord]) -> Result<(), String> {
        self.replay(vm, events)
    }
}

// ---------------------------------------------------------------------------
// Native helpers
// ---------------------------------------------------------------------------

fn type_err(vm: &mut Vm, msg: &str) -> Value {
    vm.make_error(ErrorKind::Type, msg)
}

fn nid_of(vm: &mut Vm, v: &Value) -> Option<usize> {
    match vm.get_prop(v, &PropertyKey::str("__nid")) {
        Ok(Value::Number(n)) if n >= 0.0 => Some(n as usize),
        _ => None,
    }
}

fn same_obj(a: &Value, b: &Value) -> bool {
    matches!((a, b), (Value::Object(x), Value::Object(y)) if x == y)
}

fn remove_listener(dom: &Rc<RefCell<Dom>>, node: usize, ty: &str, handler: &Value, capture: bool) {
    let mut d = dom.borrow_mut();
    if let Some(pos) = d.nodes[node]
        .listeners
        .iter()
        .position(|l| l.ty == ty && l.capture == capture && same_obj(&l.handler, handler))
    {
        d.nodes[node].listeners.remove(pos);
    }
}

fn define_accessor(target: &JsObject, name: &str, get: Value, set: Option<Value>) {
    target.borrow_mut().props.insert(
        PropertyKey::str(name),
        Property {
            kind: PropertyKind::Accessor { get: Some(get), set },
            enumerable: false,
            configurable: true,
        },
    );
}

/// Upgrade a weak DOM ref or bail out of a native call gracefully (the document
/// was dropped by the embedder; calls become no-ops returning `undefined`).
macro_rules! dom_or_return {
    ($weak:expr) => {
        match $weak.upgrade() {
            Some(d) => d,
            None => return Ok(Value::Undefined),
        }
    };
}

fn node_wrapper(vm: &mut Vm, dom: &Rc<RefCell<Dom>>, nid: usize) -> Value {
    if let Some(w) = dom.borrow().nodes.get(nid).and_then(|n| n.wrapper.clone()) {
        return w;
    }
    let obj = vm.new_object();
    vm.define_value(&obj, "__nid", Value::Number(nid as f64));
    install_node_api(vm, dom, &obj);
    let v = Value::Object(obj);
    dom.borrow_mut().nodes[nid].wrapper = Some(v.clone());
    v
}

#[allow(clippy::too_many_arguments)]
fn make_event(
    vm: &mut Vm,
    weak: &Weak<RefCell<Dom>>,
    target: usize,
    ty: &str,
    detail: &serde_json::Value,
    bubbles: bool,
    cancelable: bool,
    flags: &Rc<RefCell<EventFlags>>,
) -> Value {
    let obj = vm.new_object();
    vm.define_value(&obj, "type", Value::str(ty));
    let dj = vm.json_to_value(detail);
    vm.define_value(&obj, "detail", dj);
    vm.define_value(&obj, "bubbles", Value::Bool(bubbles));
    vm.define_value(&obj, "cancelable", Value::Bool(cancelable));
    vm.define_value(&obj, "eventPhase", Value::Number(0.0));
    if let Some(strong) = weak.upgrade() {
        let tw = node_wrapper(vm, &strong, target);
        vm.define_value(&obj, "target", tw.clone());
        vm.define_value(&obj, "currentTarget", tw);
    }

    let f = flags.clone();
    vm.define_method(&obj, "preventDefault", 0, move |_, _, _| {
        f.borrow_mut().default_prevented = true;
        Ok(Value::Undefined)
    });
    let f = flags.clone();
    vm.define_method(&obj, "stopPropagation", 0, move |_, _, _| {
        f.borrow_mut().stop_propagation = true;
        Ok(Value::Undefined)
    });
    let f = flags.clone();
    vm.define_method(&obj, "stopImmediatePropagation", 0, move |_, _, _| {
        let mut fl = f.borrow_mut();
        fl.stop_propagation = true;
        fl.stop_immediate = true;
        Ok(Value::Undefined)
    });
    // defaultPrevented getter reflects the live flag.
    let f = flags.clone();
    let get_dp = vm.new_native("get defaultPrevented", 0, move |_, _, _| {
        Ok(Value::Bool(f.borrow().default_prevented))
    });
    define_accessor(&obj, "defaultPrevented", Value::Object(get_dp), None);
    Value::Object(obj)
}

/// Install the Node/Element method + accessor surface onto a wrapper object.
/// Every closure captures only a `Weak` to the arena (see module GC note).
fn install_node_api(vm: &mut Vm, dom: &Rc<RefCell<Dom>>, obj: &JsObject) {
    let weak = Rc::downgrade(dom);

    // ---- structural mutation ----
    let d = weak.clone();
    vm.define_method(obj, "appendChild", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let parent = nid_of(vm, &this).ok_or_else(|| type_err(vm, "appendChild: not a node"))?;
        let child = nid_of(vm, args.first().unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "appendChild: argument is not a node"))?;
        dom.borrow_mut()
            .append_child(parent, child)
            .map_err(|e| type_err(vm, &e))?;
        Ok(node_wrapper(vm, &dom, child))
    });

    let d = weak.clone();
    vm.define_method(obj, "insertBefore", 2, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let parent = nid_of(vm, &this).ok_or_else(|| type_err(vm, "insertBefore: not a node"))?;
        let child = nid_of(vm, args.first().unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "insertBefore: child is not a node"))?;
        match nid_of(vm, args.get(1).unwrap_or(&Value::Undefined)) {
            Some(before) => dom
                .borrow_mut()
                .insert_before(parent, child, before)
                .map_err(|e| type_err(vm, &e))?,
            None => dom
                .borrow_mut()
                .append_child(parent, child)
                .map_err(|e| type_err(vm, &e))?,
        }
        Ok(node_wrapper(vm, &dom, child))
    });

    let d = weak.clone();
    vm.define_method(obj, "removeChild", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let parent = nid_of(vm, &this).ok_or_else(|| type_err(vm, "removeChild: not a node"))?;
        let child = nid_of(vm, args.first().unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "removeChild: argument is not a node"))?;
        if dom.borrow().nodes[child].parent != Some(parent) {
            return Err(type_err(vm, "removeChild: not a child of this node"));
        }
        dom.borrow_mut().detach(child);
        Ok(node_wrapper(vm, &dom, child))
    });

    let d = weak.clone();
    vm.define_method(obj, "replaceChild", 2, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let parent = nid_of(vm, &this).ok_or_else(|| type_err(vm, "replaceChild: not a node"))?;
        let new_c = nid_of(vm, args.first().unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "replaceChild: new child is not a node"))?;
        let old_c = nid_of(vm, args.get(1).unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "replaceChild: old child is not a node"))?;
        {
            let mut b = dom.borrow_mut();
            b.insert_before(parent, new_c, old_c).map_err(|e| type_err(vm, &e))?;
            b.detach(old_c);
        }
        Ok(node_wrapper(vm, &dom, old_c))
    });

    let d = weak.clone();
    vm.define_method(obj, "remove", 0, move |vm, this, _args| {
        let dom = dom_or_return!(d);
        if let Some(id) = nid_of(vm, &this) {
            dom.borrow_mut().detach(id);
        }
        Ok(Value::Undefined)
    });

    let d = weak.clone();
    vm.define_method(obj, "cloneNode", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "cloneNode: not a node"))?;
        let deep = vm.to_boolean(args.first().unwrap_or(&Value::Undefined));
        let new_id = dom.borrow_mut().clone_node(id, deep);
        Ok(node_wrapper(vm, &dom, new_id))
    });

    let d = weak.clone();
    vm.define_method(obj, "hasChildNodes", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "hasChildNodes: not a node"))?;
        let empty = dom.borrow().nodes[id].children.is_empty();
        Ok(Value::Bool(!empty))
    });

    // ---- attributes ----
    let d = weak.clone();
    vm.define_method(obj, "setAttribute", 2, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "setAttribute: not a node"))?;
        let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let value = vm.to_string_lossy(args.get(1).unwrap_or(&Value::Undefined));
        dom.borrow_mut().set_attribute(id, &name, &value);
        Ok(Value::Undefined)
    });

    let d = weak.clone();
    vm.define_method(obj, "getAttribute", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "getAttribute: not a node"))?;
        let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let attr = dom.borrow().get_attribute(id, &name);
        Ok(match attr {
            Some(v) => Value::str(v),
            None => Value::Null,
        })
    });

    let d = weak.clone();
    vm.define_method(obj, "hasAttribute", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "hasAttribute: not a node"))?;
        let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let has = dom.borrow().get_attribute(id, &name).is_some();
        Ok(Value::Bool(has))
    });

    let d = weak.clone();
    vm.define_method(obj, "removeAttribute", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "removeAttribute: not a node"))?;
        let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        dom.borrow_mut().remove_attribute(id, &name);
        Ok(Value::Undefined)
    });

    // ---- events ----
    let d = weak.clone();
    vm.define_method(obj, "addEventListener", 3, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "addEventListener: not a node"))?;
        let ty = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let handler = args.get(1).cloned().unwrap_or(Value::Undefined);
        if !vm.is_callable(&handler) {
            return Err(type_err(vm, "addEventListener: handler is not callable"));
        }
        let (capture, once) = parse_listener_opts(vm, args.get(2));
        // De-dupe identical (type, handler, capture) registrations, per spec.
        let mut b = dom.borrow_mut();
        let dup = b.nodes[id]
            .listeners
            .iter()
            .any(|l| l.ty == ty && l.capture == capture && same_obj(&l.handler, &handler));
        if !dup {
            b.nodes[id].listeners.push(Listener { ty, handler, capture, once });
        }
        Ok(Value::Undefined)
    });

    let d = weak.clone();
    vm.define_method(obj, "removeEventListener", 3, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "removeEventListener: not a node"))?;
        let ty = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let handler = args.get(1).cloned().unwrap_or(Value::Undefined);
        let (capture, _once) = parse_listener_opts(vm, args.get(2));
        remove_listener(&dom, id, &ty, &handler, capture);
        Ok(Value::Undefined)
    });

    let d = weak.clone();
    vm.define_method(obj, "dispatchEvent", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "dispatchEvent: not a node"))?;
        let ev = args.first().cloned().unwrap_or(Value::Undefined);
        let ty_v = vm_get(vm, &ev, "type");
        let ty = vm.to_string_lossy(&ty_v);
        let detail_v = vm_get(vm, &ev, "detail");
        let detail = vm.value_to_json(&detail_v);
        let bubbles_v = vm_get(vm, &ev, "bubbles");
        let bubbles = vm.to_boolean(&bubbles_v);
        let cancelable_v = vm_get(vm, &ev, "cancelable");
        let cancelable = vm.to_boolean(&cancelable_v);
        let handle = DomHandle(dom.clone());
        let ok = handle
            .dispatch_event_opts(vm, id, &ty, detail, bubbles, cancelable)
            .map_err(|e| type_err(vm, &e))?;
        Ok(Value::Bool(ok))
    });

    // ---- measurement (captured reads) ----
    let d = weak.clone();
    vm.define_method(obj, "getBoundingClientRect", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "getBoundingClientRect: not a node"))?;
        let v = dom.borrow_mut().measure(id, "getBoundingClientRect");
        Ok(vm.json_to_value(&v))
    });
    for prop in ["offsetWidth", "offsetHeight", "clientWidth", "clientHeight", "scrollWidth", "scrollHeight"] {
        let d = weak.clone();
        let getter = vm.new_native(&format!("get {prop}"), 0, move |vm, this, _| {
            let dom = match d.upgrade() {
                Some(x) => x,
                None => return Ok(Value::Number(0.0)),
            };
            let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "measure: not a node"))?;
            let v = dom.borrow_mut().measure(id, prop);
            Ok(match v {
                serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
                _ => Value::Number(0.0),
            })
        });
        define_accessor(obj, prop, Value::Object(getter), None);
    }

    // ---- text / reflected attributes ----
    install_text_accessor(vm, &weak, obj);
    install_reflected_attr(vm, &weak, obj, "id", "id");
    install_reflected_attr(vm, &weak, obj, "className", "class");

    // ---- read-only structural accessors ----
    install_tag_accessors(vm, &weak, obj);
    install_tree_accessors(vm, &weak, obj);

    // ---- classList ----
    install_classlist(vm, &weak, obj);

    // ---- selectors ----
    install_query(vm, &weak, obj);

    // ---- innerHTML / outerHTML (getters) ----
    let d = weak.clone();
    let get_inner = vm.new_native("get innerHTML", 0, move |vm, this, _| {
        let dom = match d.upgrade() {
            Some(x) => x,
            None => return Ok(Value::str("")),
        };
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "innerHTML: not a node"))?;
        let html = dom.borrow().render_children(id);
        Ok(Value::str(html))
    });
    let d = weak.clone();
    let set_inner = vm.new_native("set innerHTML", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "innerHTML: not a node"))?;
        let html = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        dom.borrow_mut().set_inner_html(id, &html);
        Ok(Value::Undefined)
    });
    define_accessor(obj, "innerHTML", Value::Object(get_inner), Some(Value::Object(set_inner)));
    let d = weak.clone();
    let get_outer = vm.new_native("get outerHTML", 0, move |vm, this, _| {
        let dom = match d.upgrade() {
            Some(x) => x,
            None => return Ok(Value::str("")),
        };
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "outerHTML: not a node"))?;
        let html = dom.borrow().render_html(id);
        Ok(Value::str(html))
    });
    define_accessor(obj, "outerHTML", Value::Object(get_outer), None);
}

fn vm_get(vm: &mut Vm, v: &Value, key: &str) -> Value {
    vm.get_prop(v, &PropertyKey::str(key)).unwrap_or(Value::Undefined)
}

fn parse_listener_opts(vm: &mut Vm, arg: Option<&Value>) -> (bool, bool) {
    match arg {
        Some(Value::Bool(b)) => (*b, false),
        Some(v @ Value::Object(_)) => {
            let cap_v = vm_get(vm, v, "capture");
            let capture = vm.to_boolean(&cap_v);
            let once_v = vm_get(vm, v, "once");
            let once = vm.to_boolean(&once_v);
            (capture, once)
        }
        _ => (false, false),
    }
}

fn install_text_accessor(vm: &mut Vm, weak: &Weak<RefCell<Dom>>, obj: &JsObject) {
    let d = weak.clone();
    let get = vm.new_native("get textContent", 0, move |vm, this, _| {
        let dom = match d.upgrade() {
            Some(x) => x,
            None => return Ok(Value::str("")),
        };
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "textContent: not a node"))?;
        let text = dom.borrow().text_content(id);
        Ok(Value::str(text))
    });
    let d = weak.clone();
    let set = vm.new_native("set textContent", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "textContent: not a node"))?;
        let data = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        dom.borrow_mut().set_text_content(id, &data);
        Ok(Value::Undefined)
    });
    define_accessor(obj, "textContent", Value::Object(get), Some(Value::Object(set)));
}

fn install_reflected_attr(vm: &mut Vm, weak: &Weak<RefCell<Dom>>, obj: &JsObject, prop: &'static str, attr: &'static str) {
    let d = weak.clone();
    let get = vm.new_native(&format!("get {prop}"), 0, move |vm, this, _| {
        let dom = match d.upgrade() {
            Some(x) => x,
            None => return Ok(Value::str("")),
        };
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "reflect: not a node"))?;
        let v = dom.borrow().get_attribute(id, attr).unwrap_or_default();
        Ok(Value::str(v))
    });
    let d = weak.clone();
    let set = vm.new_native(&format!("set {prop}"), 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "reflect: not a node"))?;
        let v = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        dom.borrow_mut().set_attribute(id, attr, &v);
        Ok(Value::Undefined)
    });
    define_accessor(obj, prop, Value::Object(get), Some(Value::Object(set)));
}

fn install_tag_accessors(vm: &mut Vm, weak: &Weak<RefCell<Dom>>, obj: &JsObject) {
    let d = weak.clone();
    let get_tag = vm.new_native("get tagName", 0, move |vm, this, _| {
        let dom = match d.upgrade() {
            Some(x) => x,
            None => return Ok(Value::Undefined),
        };
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "tagName: not a node"))?;
        let kind = dom.borrow().nodes[id].kind.clone();
        Ok(match kind {
            NodeKind::Element(t) => Value::str(t.to_uppercase()),
            _ => Value::Undefined,
        })
    });
    define_accessor(obj, "tagName", Value::Object(get_tag.clone()), None);
    define_accessor(obj, "nodeName", Value::Object(get_tag), None);

    let d = weak.clone();
    let get_type = vm.new_native("get nodeType", 0, move |vm, this, _| {
        let dom = match d.upgrade() {
            Some(x) => x,
            None => return Ok(Value::Number(0.0)),
        };
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "nodeType: not a node"))?;
        let kind = dom.borrow().nodes[id].kind.clone();
        Ok(Value::Number(match kind {
            NodeKind::Element(_) => 1.0,
            NodeKind::Text => 3.0,
            NodeKind::Document => 9.0,
        }))
    });
    define_accessor(obj, "nodeType", Value::Object(get_type), None);
}

fn install_tree_accessors(vm: &mut Vm, weak: &Weak<RefCell<Dom>>, obj: &JsObject) {
    // parentNode
    let d = weak.clone();
    let get_parent = vm.new_native("get parentNode", 0, move |vm, this, _| {
        let dom = match d.upgrade() {
            Some(x) => x,
            None => return Ok(Value::Null),
        };
        let id = match nid_of(vm, &this) {
            Some(id) => id,
            None => return Ok(Value::Null),
        };
        let parent = dom.borrow().nodes[id].parent;
        Ok(match parent {
            Some(p) => node_wrapper(vm, &dom, p),
            None => Value::Null,
        })
    });
    define_accessor(obj, "parentNode", Value::Object(get_parent.clone()), None);
    define_accessor(obj, "parentElement", Value::Object(get_parent), None);

    // childNodes (all) and children (elements only)
    let d = weak.clone();
    let get_child_nodes = vm.new_native("get childNodes", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "childNodes: not a node"))?;
        let kids = dom.borrow().nodes[id].children.clone();
        let vals: Vec<Value> = kids.into_iter().map(|c| node_wrapper(vm, &dom, c)).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });
    define_accessor(obj, "childNodes", Value::Object(get_child_nodes), None);

    let d = weak.clone();
    let get_children = vm.new_native("get children", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "children: not a node"))?;
        let kids: Vec<usize> = {
            let b = dom.borrow();
            b.nodes[id]
                .children
                .iter()
                .copied()
                .filter(|&c| matches!(b.nodes[c].kind, NodeKind::Element(_)))
                .collect()
        };
        let vals: Vec<Value> = kids.into_iter().map(|c| node_wrapper(vm, &dom, c)).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });
    define_accessor(obj, "children", Value::Object(get_children), None);

    // firstChild / lastChild
    let d = weak.clone();
    let get_first = vm.new_native("get firstChild", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "firstChild: not a node"))?;
        let first = dom.borrow().nodes[id].children.first().copied();
        Ok(match first {
            Some(c) => node_wrapper(vm, &dom, c),
            None => Value::Null,
        })
    });
    define_accessor(obj, "firstChild", Value::Object(get_first), None);

    let d = weak.clone();
    let get_last = vm.new_native("get lastChild", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "lastChild: not a node"))?;
        let last = dom.borrow().nodes[id].children.last().copied();
        Ok(match last {
            Some(c) => node_wrapper(vm, &dom, c),
            None => Value::Null,
        })
    });
    define_accessor(obj, "lastChild", Value::Object(get_last), None);

    // nextSibling / previousSibling
    let d = weak.clone();
    let get_next = vm.new_native("get nextSibling", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "nextSibling: not a node"))?;
        let sib = sibling(&dom.borrow(), id, 1);
        Ok(match sib {
            Some(c) => node_wrapper(vm, &dom, c),
            None => Value::Null,
        })
    });
    define_accessor(obj, "nextSibling", Value::Object(get_next), None);

    let d = weak.clone();
    let get_prev = vm.new_native("get previousSibling", 0, move |vm, this, _| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "previousSibling: not a node"))?;
        let sib = sibling(&dom.borrow(), id, -1);
        Ok(match sib {
            Some(c) => node_wrapper(vm, &dom, c),
            None => Value::Null,
        })
    });
    define_accessor(obj, "previousSibling", Value::Object(get_prev), None);
}

fn sibling(dom: &Dom, id: usize, delta: i64) -> Option<usize> {
    let parent = dom.nodes[id].parent?;
    let sibs = &dom.nodes[parent].children;
    let pos = sibs.iter().position(|&c| c == id)? as i64 + delta;
    if pos < 0 || pos as usize >= sibs.len() {
        None
    } else {
        Some(sibs[pos as usize])
    }
}

fn install_classlist(vm: &mut Vm, weak: &Weak<RefCell<Dom>>, obj: &JsObject) {
    let d = weak.clone();
    let get = vm.new_native("get classList", 0, move |vm, this, _| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "classList: not a node"))?;
        let list = vm.new_object();
        vm.define_value(&list, "__nid", Value::Number(id as f64));

        let dd = d.clone();
        vm.define_method(&list, "add", 1, move |vm, _t, args| {
            let dom = dom_or_return!(dd);
            let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
            let mut b = dom.borrow_mut();
            let mut cls = b.classes(id);
            if !cls.contains(&name) {
                cls.push(name);
                b.set_classes(id, &cls);
            }
            Ok(Value::Undefined)
        });
        let dd = d.clone();
        vm.define_method(&list, "remove", 1, move |vm, _t, args| {
            let dom = dom_or_return!(dd);
            let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
            let mut b = dom.borrow_mut();
            let mut cls = b.classes(id);
            cls.retain(|c| c != &name);
            b.set_classes(id, &cls);
            Ok(Value::Undefined)
        });
        let dd = d.clone();
        vm.define_method(&list, "toggle", 1, move |vm, _t, args| {
            let dom = dom_or_return!(dd);
            let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
            let mut b = dom.borrow_mut();
            let mut cls = b.classes(id);
            let present = if cls.contains(&name) {
                cls.retain(|c| c != &name);
                false
            } else {
                cls.push(name);
                true
            };
            b.set_classes(id, &cls);
            Ok(Value::Bool(present))
        });
        let dd = d.clone();
        vm.define_method(&list, "contains", 1, move |vm, _t, args| {
            let dom = match dd.upgrade() {
                Some(x) => x,
                None => return Ok(Value::Bool(false)),
            };
            let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
            let has = dom.borrow().classes(id).contains(&name);
            Ok(Value::Bool(has))
        });
        Ok(Value::Object(list))
    });
    define_accessor(obj, "classList", Value::Object(get), None);
}

fn install_query(vm: &mut Vm, weak: &Weak<RefCell<Dom>>, obj: &JsObject) {
    let d = weak.clone();
    vm.define_method(obj, "querySelector", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "querySelector: not a node"))?;
        let sel = parse_selector(&vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined)));
        let found = dom.borrow().select(id, &sel, true).first().copied();
        Ok(match found {
            Some(c) => node_wrapper(vm, &dom, c),
            None => Value::Null,
        })
    });
    let d = weak.clone();
    vm.define_method(obj, "querySelectorAll", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "querySelectorAll: not a node"))?;
        let sel = parse_selector(&vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined)));
        let found = dom.borrow().select(id, &sel, false);
        let vals: Vec<Value> = found.into_iter().map(|c| node_wrapper(vm, &dom, c)).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });
    let d = weak.clone();
    vm.define_method(obj, "getElementsByTagName", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "getElementsByTagName: not a node"))?;
        let tag = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let sel = Selector { tag: Some(tag), id: None, classes: vec![] };
        let found = dom.borrow().select(id, &sel, false);
        let vals: Vec<Value> = found.into_iter().map(|c| node_wrapper(vm, &dom, c)).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });
    let d = weak.clone();
    vm.define_method(obj, "getElementsByClassName", 1, move |vm, this, args| {
        let dom = dom_or_return!(d);
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "getElementsByClassName: not a node"))?;
        let cls = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let sel = Selector { tag: None, id: None, classes: vec![cls] };
        let found = dom.borrow().select(id, &sel, false);
        let vals: Vec<Value> = found.into_iter().map(|c| node_wrapper(vm, &dom, c)).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install `document` and `window` globals backed by a fresh virtual DOM, and
/// return the strong [`DomHandle`] the embedder drives. Call once per VM.
pub fn install(vm: &mut Vm) -> DomHandle {
    let dom = Rc::new(RefCell::new(Dom::new()));
    let weak = Rc::downgrade(&dom);
    let document = vm.new_object();

    let d = weak.clone();
    vm.define_method(&document, "createElement", 1, move |vm, _this, args| {
        let dom = dom_or_return!(d);
        let tag = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let id = dom.borrow_mut().create_element(&tag);
        Ok(node_wrapper(vm, &dom, id))
    });

    let d = weak.clone();
    vm.define_method(&document, "createTextNode", 1, move |vm, _this, args| {
        let dom = dom_or_return!(d);
        let data = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let id = dom.borrow_mut().create_text(&data);
        Ok(node_wrapper(vm, &dom, id))
    });

    let d = weak.clone();
    vm.define_method(&document, "getElementById", 1, move |vm, _this, args| {
        let dom = dom_or_return!(d);
        let dom_id = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let found = dom.borrow().find_by_id(&dom_id);
        Ok(match found {
            Some(id) => node_wrapper(vm, &dom, id),
            None => Value::Null,
        })
    });

    // document.querySelector(All) / getElementsBy* operate from <html>.
    install_query(vm, &weak, &document);

    let d = weak.clone();
    let get_body = vm.new_native("get body", 0, move |vm, _this, _| {
        let dom = dom_or_return!(d);
        let body = dom.borrow().body;
        Ok(node_wrapper(vm, &dom, body))
    });
    define_accessor(&document, "body", Value::Object(get_body), None);

    let d = weak.clone();
    let get_doc_el = vm.new_native("get documentElement", 0, move |vm, _this, _| {
        let dom = dom_or_return!(d);
        let html = dom.borrow().html;
        Ok(node_wrapper(vm, &dom, html))
    });
    define_accessor(&document, "documentElement", Value::Object(get_doc_el), None);

    // document is itself a node (so querySelector starts at <html>).
    {
        let html = dom.borrow().html;
        vm.define_value(&document, "__nid", Value::Number(html as f64));
    }

    let global = vm.realm.global.clone();
    vm.define_value(&global, "document", Value::Object(document.clone()));

    let window = vm.new_object();
    vm.define_value(&window, "document", Value::Object(document));
    vm.define_value(&global, "window", Value::Object(window));

    DomHandle(dom)
}
