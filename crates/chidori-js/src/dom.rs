//! A deterministic, replayable virtual DOM for the pure-Rust engine.
//!
//! This is the "DOM behind the host boundary" experiment: instead of a browser
//! owning a mutable document and the engine poking at it over FFI, the document
//! is a Rust-side arena that the engine drives through ordinary DOM-shaped
//! JavaScript (`document.createElement`, `el.appendChild`, `el.textContent`,
//! `el.addEventListener`, …). Two journals fall out of that design for free:
//!
//! * **Mutation log** (output): every structural / attribute / text change is
//!   appended to an ordered, serializable [`Mutation`] stream. That stream *is*
//!   the render protocol — ship it to a dumb browser/canvas renderer, or diff it
//!   against a prior run. It is also fully deterministic: node ids are assigned
//!   by sequential allocation, attribute order is insertion order, so the same
//!   program + the same inputs always produce the byte-identical stream.
//!
//! * **Event log** (input): every event delivered into the document via
//!   [`DomHandle::dispatch_event`] is appended to an ordered [`EventRecord`]
//!   stream. Replaying that stream reproduces the mutation stream exactly — which
//!   is the property that makes time-travel, fork-and-edit-rerun, and "record a
//!   session, replay it as a test" possible. Events are the *only* source of
//!   non-determinism, and they live in the journal.
//!
//! The integration with Chidori's durable host (`crates/chidori-js` →
//! `install_chidori_effects`) is the natural next layer: a DOM mutation batch is
//! flushed as a captured host effect, and an inbound UI event is a captured host
//! input — exactly the taxonomy [`crate::Engine::install_chidori_effects`]
//! already uses for `prompt`, `tool`, and `fetch`. See
//! `docs/dom-runtime-prototype.md`.
//!
//! Prototype scope / known gaps (documented honestly, not hidden):
//! * No layout, CSS, or measurement reads (`getBoundingClientRect` etc.) — those
//!   are the *other* captured-effect direction and are out of scope here.
//! * Node wrapper objects are cached on the node for stable JS identity
//!   (`el.parentNode === container` holds); that cache forms an Rc cycle with the
//!   native closures it holds, so the document arena is freed at session end, not
//!   incrementally. A production version would hold wrappers via GC-traced slots.
//! * Event dispatch bubbles target→root and ignores capture phase / `stopPropagation`.

use crate::value::{JsObject, Property, PropertyKey, PropertyKind, Value};
use crate::vm::{ErrorKind, Vm};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;

/// A single change to the document. The ordered stream of these is the render
/// protocol and the deterministic output journal.
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

/// One delivered UI event. The ordered stream of these is the input journal;
/// replaying it reproduces the mutation stream exactly.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventRecord {
    pub target: usize,
    #[serde(rename = "type")]
    pub ty: String,
    pub detail: serde_json::Value,
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
    /// Attributes in insertion order (deterministic serialization).
    attrs: Vec<(String, String)>,
    /// Character data for text nodes.
    text: String,
    /// Registered (event-type, handler) pairs. Holding the handler `Value` keeps
    /// the closure (and its captured environment) alive across event dispatches.
    listeners: Vec<(String, Value)>,
    /// Cached JS wrapper for stable identity (see module-level note on the cycle).
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

/// The virtual document arena. Nodes are addressed by their index (`NodeId`),
/// assigned by sequential allocation so ids are stable and replay-deterministic.
pub struct Dom {
    nodes: Vec<NodeData>,
    document: usize,
    html: usize,
    body: usize,
    mutations: Vec<Mutation>,
    events: Vec<EventRecord>,
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
        // Bootstrap mutations so a renderer can build the initial frame.
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

    /// Detach `child` from its current parent (no-op if it has none).
    fn detach(&mut self, child: usize) {
        if let Some(parent) = self.nodes[child].parent {
            self.nodes[parent].children.retain(|&c| c != child);
            self.nodes[child].parent = None;
            self.mutations.push(Mutation::Remove { parent, child });
        }
    }

    fn append_child(&mut self, parent: usize, child: usize) {
        self.detach(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
        self.mutations.push(Mutation::Append { parent, child });
    }

    fn insert_before(&mut self, parent: usize, child: usize, before: usize) -> bool {
        let Some(idx) = self.nodes[parent].children.iter().position(|&c| c == before) else {
            return false;
        };
        self.detach(child);
        // Recompute index — detach may have shifted it if child shared the parent.
        let idx = self
            .nodes[parent]
            .children
            .iter()
            .position(|&c| c == before)
            .unwrap_or(idx);
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.insert(idx, child);
        self.mutations.push(Mutation::InsertBefore { parent, child, before });
        true
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
        self.nodes[id]
            .attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }

    fn remove_attribute(&mut self, id: usize, name: &str) {
        let before = self.nodes[id].attrs.len();
        self.nodes[id].attrs.retain(|(n, _)| n != name);
        if self.nodes[id].attrs.len() != before {
            self.mutations
                .push(Mutation::RemoveAttribute { id, name: name.to_string() });
        }
    }

    /// `textContent` setter semantics: replace all children with a single text
    /// node (or none, if empty). For a text node it just rewrites the data.
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
            self.append_child(id, t);
        }
    }

    /// `textContent` getter: concatenation of all descendant text data.
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

    fn find_by_id(&self, dom_id: &str) -> Option<usize> {
        self.nodes.iter().enumerate().find_map(|(i, n)| {
            n.attrs
                .iter()
                .any(|(k, v)| k == "id" && v == dom_id)
                .then_some(i)
        })
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
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// A cloneable handle to the live document, held by the embedder (the host loop).
/// This is the seam the durable host drives: drain mutations to render, push
/// events to drive interaction, snapshot to persist.
#[derive(Clone)]
pub struct DomHandle(Rc<RefCell<Dom>>);

impl DomHandle {
    /// The body element's node id (the usual mount point).
    pub fn body_id(&self) -> usize {
        self.0.borrow().body
    }

    /// Resolve an element by its `id` attribute (host-side query).
    pub fn element_by_id(&self, dom_id: &str) -> Option<usize> {
        self.0.borrow().find_by_id(dom_id)
    }

    /// All mutations recorded so far (the render journal). Cloned, not drained.
    pub fn mutations(&self) -> Vec<Mutation> {
        self.0.borrow().mutations.clone()
    }

    /// Take and clear the pending mutations — the incremental render batch.
    pub fn drain_mutations(&self) -> Vec<Mutation> {
        std::mem::take(&mut self.0.borrow_mut().mutations)
    }

    /// The recorded input event journal.
    pub fn events(&self) -> Vec<EventRecord> {
        self.0.borrow().events.clone()
    }

    /// Serialize the current document subtree under `<html>` to HTML — a cheap,
    /// deterministic way to assert on / visualize rendered state.
    pub fn render_html(&self) -> String {
        let dom = self.0.borrow();
        dom.render_html(dom.html)
    }

    /// Deliver an event to `target`, recording it in the journal and invoking
    /// every matching listener along the bubble path (target → root). Handlers
    /// run with a borrow-free DOM so they may mutate the document re-entrantly.
    pub fn dispatch_event(
        &self,
        vm: &mut Vm,
        target: usize,
        ty: &str,
        detail: serde_json::Value,
    ) -> Result<(), String> {
        self.0.borrow_mut().events.push(EventRecord {
            target,
            ty: ty.to_string(),
            detail: detail.clone(),
        });
        // Collect handlers up the ancestor chain *before* invoking any, so a
        // handler that mutates listeners can't corrupt the in-flight dispatch.
        let mut handlers: Vec<Value> = Vec::new();
        {
            let dom = self.0.borrow();
            if target >= dom.nodes.len() {
                return Err(format!("dispatch_event: no node #{target}"));
            }
            let mut cur = Some(target);
            while let Some(id) = cur {
                for (t, h) in &dom.nodes[id].listeners {
                    if t == ty {
                        handlers.push(h.clone());
                    }
                }
                cur = dom.nodes[id].parent;
            }
        }
        let event = make_event(vm, &self.0, target, ty, &detail);
        for h in handlers {
            if let Err(e) = vm.call(h, Value::Undefined, &[event.clone()]) {
                let msg = vm.error_to_string(&e);
                return Err(format!("event handler threw: {msg}"));
            }
        }
        let _ = vm.run_jobs_until_blocked();
        Ok(())
    }

    /// Replay a recorded event journal against this (fresh) document. Given the
    /// same built DOM, this reproduces the mutation journal exactly.
    pub fn replay_events(&self, vm: &mut Vm, events: &[EventRecord]) -> Result<(), String> {
        for ev in events {
            self.dispatch_event(vm, ev.target, &ev.ty, ev.detail.clone())?;
        }
        Ok(())
    }
}

fn type_err(vm: &mut Vm, msg: &str) -> Value {
    vm.make_error(ErrorKind::Type, msg)
}

/// Read the hidden `__nid` node id off a wrapper value, if it is one.
fn nid_of(vm: &mut Vm, v: &Value) -> Option<usize> {
    match vm.get_prop(v, &PropertyKey::str("__nid")) {
        Ok(Value::Number(n)) if n >= 0.0 => Some(n as usize),
        _ => None,
    }
}

fn define_accessor(vm: &Vm, target: &JsObject, name: &str, get: Value, set: Option<Value>) {
    target.borrow_mut().props.insert(
        PropertyKey::str(name),
        Property {
            kind: PropertyKind::Accessor { get: Some(get), set },
            enumerable: false,
            configurable: true,
        },
    );
}

/// Get-or-create the cached JS wrapper for a node (stable identity).
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

fn make_event(
    vm: &mut Vm,
    dom: &Rc<RefCell<Dom>>,
    target: usize,
    ty: &str,
    detail: &serde_json::Value,
) -> Value {
    let obj = vm.new_object();
    vm.define_value(&obj, "type", Value::str(ty));
    let dj = vm.json_to_value(detail);
    vm.define_value(&obj, "detail", dj);
    let tw = node_wrapper(vm, dom, target);
    vm.define_value(&obj, "target", tw.clone());
    vm.define_value(&obj, "currentTarget", tw);
    vm.define_method(&obj, "preventDefault", 0, |_, _, _| Ok(Value::Undefined));
    vm.define_method(&obj, "stopPropagation", 0, |_, _, _| Ok(Value::Undefined));
    Value::Object(obj)
}

/// Install the Node/Element method + accessor surface onto a wrapper object.
fn install_node_api(vm: &mut Vm, dom: &Rc<RefCell<Dom>>, obj: &JsObject) {
    // --- methods ---
    let d = dom.clone();
    vm.define_method(obj, "appendChild", 1, move |vm, this, args| {
        let parent = nid_of(vm, &this).ok_or_else(|| type_err(vm, "appendChild: not a node"))?;
        let child = nid_of(vm, args.first().unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "appendChild: argument is not a node"))?;
        d.borrow_mut().append_child(parent, child);
        Ok(node_wrapper(vm, &d, child))
    });

    let d = dom.clone();
    vm.define_method(obj, "insertBefore", 2, move |vm, this, args| {
        let parent = nid_of(vm, &this).ok_or_else(|| type_err(vm, "insertBefore: not a node"))?;
        let child = nid_of(vm, args.first().unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "insertBefore: child is not a node"))?;
        match nid_of(vm, args.get(1).unwrap_or(&Value::Undefined)) {
            Some(before) => {
                d.borrow_mut().insert_before(parent, child, before);
            }
            None => {
                // insertBefore(child, null) === appendChild(child)
                d.borrow_mut().append_child(parent, child);
            }
        }
        Ok(node_wrapper(vm, &d, child))
    });

    let d = dom.clone();
    vm.define_method(obj, "removeChild", 1, move |vm, this, args| {
        let parent = nid_of(vm, &this).ok_or_else(|| type_err(vm, "removeChild: not a node"))?;
        let child = nid_of(vm, args.first().unwrap_or(&Value::Undefined))
            .ok_or_else(|| type_err(vm, "removeChild: argument is not a node"))?;
        if d.borrow().nodes[child].parent != Some(parent) {
            return Err(type_err(vm, "removeChild: not a child of this node"));
        }
        d.borrow_mut().detach(child);
        Ok(node_wrapper(vm, &d, child))
    });

    let d = dom.clone();
    vm.define_method(obj, "remove", 0, move |vm, this, _args| {
        if let Some(id) = nid_of(vm, &this) {
            d.borrow_mut().detach(id);
        }
        Ok(Value::Undefined)
    });

    let d = dom.clone();
    vm.define_method(obj, "setAttribute", 2, move |vm, this, args| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "setAttribute: not a node"))?;
        let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let value = vm.to_string_lossy(args.get(1).unwrap_or(&Value::Undefined));
        d.borrow_mut().set_attribute(id, &name, &value);
        Ok(Value::Undefined)
    });

    let d = dom.clone();
    vm.define_method(obj, "getAttribute", 1, move |vm, this, args| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "getAttribute: not a node"))?;
        let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        Ok(match d.borrow().get_attribute(id, &name) {
            Some(v) => Value::str(v),
            None => Value::Null,
        })
    });

    let d = dom.clone();
    vm.define_method(obj, "removeAttribute", 1, move |vm, this, args| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "removeAttribute: not a node"))?;
        let name = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        d.borrow_mut().remove_attribute(id, &name);
        Ok(Value::Undefined)
    });

    let d = dom.clone();
    vm.define_method(obj, "addEventListener", 2, move |vm, this, args| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "addEventListener: not a node"))?;
        let ty = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let handler = args.get(1).cloned().unwrap_or(Value::Undefined);
        if !vm.is_callable(&handler) {
            return Err(type_err(vm, "addEventListener: handler is not callable"));
        }
        d.borrow_mut().nodes[id].listeners.push((ty, handler));
        Ok(Value::Undefined)
    });

    // --- accessors ---
    let d = dom.clone();
    let get_text = vm.new_native("get textContent", 0, move |vm, this, _| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "textContent: not a node"))?;
        Ok(Value::str(d.borrow().text_content(id)))
    });
    let d = dom.clone();
    let set_text = vm.new_native("set textContent", 1, move |vm, this, args| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "textContent: not a node"))?;
        let data = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        d.borrow_mut().set_text_content(id, &data);
        Ok(Value::Undefined)
    });
    define_accessor(vm, obj, "textContent", Value::Object(get_text), Some(Value::Object(set_text)));

    // `id` reflects the "id" attribute.
    let d = dom.clone();
    let get_id = vm.new_native("get id", 0, move |vm, this, _| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "id: not a node"))?;
        Ok(Value::str(d.borrow().get_attribute(id, "id").unwrap_or_default()))
    });
    let d = dom.clone();
    let set_id = vm.new_native("set id", 1, move |vm, this, args| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "id: not a node"))?;
        let v = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        d.borrow_mut().set_attribute(id, "id", &v);
        Ok(Value::Undefined)
    });
    define_accessor(vm, obj, "id", Value::Object(get_id), Some(Value::Object(set_id)));

    // `className` reflects the "class" attribute.
    let d = dom.clone();
    let get_cls = vm.new_native("get className", 0, move |vm, this, _| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "className: not a node"))?;
        Ok(Value::str(d.borrow().get_attribute(id, "class").unwrap_or_default()))
    });
    let d = dom.clone();
    let set_cls = vm.new_native("set className", 1, move |vm, this, args| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "className: not a node"))?;
        let v = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        d.borrow_mut().set_attribute(id, "class", &v);
        Ok(Value::Undefined)
    });
    define_accessor(vm, obj, "className", Value::Object(get_cls), Some(Value::Object(set_cls)));

    // `tagName` (uppercase, read-only).
    let d = dom.clone();
    let get_tag = vm.new_native("get tagName", 0, move |vm, this, _| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "tagName: not a node"))?;
        Ok(match &d.borrow().nodes[id].kind {
            NodeKind::Element(t) => Value::str(t.to_uppercase()),
            _ => Value::Undefined,
        })
    });
    define_accessor(vm, obj, "tagName", Value::Object(get_tag), None);

    // `parentNode` (read-only).
    let d = dom.clone();
    let get_parent = vm.new_native("get parentNode", 0, move |vm, this, _| {
        let id = match nid_of(vm, &this) {
            Some(id) => id,
            None => return Ok(Value::Null),
        };
        let parent = d.borrow().nodes[id].parent;
        Ok(match parent {
            Some(p) => node_wrapper(vm, &d, p),
            None => Value::Null,
        })
    });
    define_accessor(vm, obj, "parentNode", Value::Object(get_parent), None);

    // `childNodes` (read-only snapshot array).
    let d = dom.clone();
    let get_children = vm.new_native("get childNodes", 0, move |vm, this, _| {
        let id = nid_of(vm, &this).ok_or_else(|| type_err(vm, "childNodes: not a node"))?;
        let kids = d.borrow().nodes[id].children.clone();
        let vals: Vec<Value> = kids.into_iter().map(|c| node_wrapper(vm, &d, c)).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });
    define_accessor(vm, obj, "childNodes", Value::Object(get_children), None);
}

/// Install `document` and `window` globals backed by a fresh virtual DOM, and
/// return the [`DomHandle`] the embedder drives. Call once per VM.
pub fn install(vm: &mut Vm) -> DomHandle {
    let dom = Rc::new(RefCell::new(Dom::new()));
    let document = vm.new_object();

    let d = dom.clone();
    vm.define_method(&document, "createElement", 1, move |vm, _this, args| {
        let tag = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let id = d.borrow_mut().create_element(&tag);
        Ok(node_wrapper(vm, &d, id))
    });

    let d = dom.clone();
    vm.define_method(&document, "createTextNode", 1, move |vm, _this, args| {
        let data = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        let id = d.borrow_mut().create_text(&data);
        Ok(node_wrapper(vm, &d, id))
    });

    let d = dom.clone();
    vm.define_method(&document, "getElementById", 1, move |vm, _this, args| {
        let dom_id = vm.to_string_lossy(args.first().unwrap_or(&Value::Undefined));
        Ok(match d.borrow().find_by_id(&dom_id) {
            Some(id) => node_wrapper(vm, &d, id),
            None => Value::Null,
        })
    });

    // document.body / document.documentElement accessors.
    let d = dom.clone();
    let get_body = vm.new_native("get body", 0, move |vm, _this, _| {
        let body = d.borrow().body;
        Ok(node_wrapper(vm, &d, body))
    });
    define_accessor(vm, &document, "body", Value::Object(get_body), None);

    let d = dom.clone();
    let get_doc_el = vm.new_native("get documentElement", 0, move |vm, _this, _| {
        let html = d.borrow().html;
        Ok(node_wrapper(vm, &d, html))
    });
    define_accessor(vm, &document, "documentElement", Value::Object(get_doc_el), None);

    let global = vm.realm.global.clone();
    vm.define_value(&global, "document", Value::Object(document.clone()));

    // A minimal `window` exposing `document`; many UI snippets reference it.
    let window = vm.new_object();
    vm.define_value(&window, "document", Value::Object(document));
    vm.define_value(&global, "window", Value::Object(window));

    DomHandle(dom)
}
