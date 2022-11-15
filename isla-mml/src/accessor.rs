// BSD 2-Clause License
//
// Copyright (c) 2022 Alasdair Armstrong
//
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
// 1. Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright
// notice, this list of conditions and the following disclaimer in the
// documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Accessors are paths into the events generated by Isla in a
//! trace. An Isla event may contain arbitrary data provided by the
//! Sail model, so we need some way to access that data from the SMT
//! memory model.

use std::borrow::Borrow;
use std::collections::HashMap;

use isla_lib::bitvector::BV;
use isla_lib::log;
use isla_lib::ir::{SharedState, Val};
use isla_lib::smt::{Event, Sym};
use isla_lib::smt::smtlib::Ty;
use isla_lib::zencode;

use crate::memory_model::constants::*;
use crate::memory_model::{Accessor, Name, Symtab};
use crate::smt::{Sexp, SexpArena, SexpId};

pub trait ModelEvent<'ev, B> {
    fn name(&self) -> Name;

    fn base_events(&self) -> &[&'ev Event<B>];

    fn base(&self) -> Option<&'ev Event<B>> {
        match self.base_events() {
            &[ev] => Some(ev),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum AccessorTree<'a> {
    Node { elem: &'a Accessor, child: Box<AccessorTree<'a>> },
    Match { arms: HashMap<Option<Name>, AccessorTree<'a>> },
    Leaf,
}

static ACCESSORTREE_LEAF: AccessorTree<'static> = AccessorTree::Leaf;

impl<'a> AccessorTree<'a> {
    pub fn from_accessors(accessors: &'a [Accessor]) -> Self {
        let mut constructor_stack = Vec::new();
        let mut cur = AccessorTree::Leaf;

        for accessor in accessors {
            match accessor {
                Accessor::Ctor(ctor) => {
                    constructor_stack.push((Some(*ctor), cur));
                    cur = AccessorTree::Leaf
                }
                Accessor::Wildcard => {
                    constructor_stack.push((None, cur));
                    cur = AccessorTree::Leaf
                }
                Accessor::Match(n) => {
                    let mut arms = constructor_stack.split_off(constructor_stack.len() - n);
                    cur = AccessorTree::Match { arms: arms.drain(..).collect() }
                }
                acc => cur = AccessorTree::Node { elem: acc, child: Box::new(cur) },
            }
        }

        assert!(constructor_stack.is_empty());

        cur
    }
}

#[derive(Copy, Clone, Debug)]
enum View<'ev, B> {
    Val(&'ev Val<B>),
    Bits(B),
    Sexp(SexpId),
}

impl<'ev, B: BV> View<'ev, B> {
    fn to_sexp(self, sexps: &mut SexpArena) -> Option<SexpId> {
        Some(match self {
            View::Sexp(id) => id,
            View::Bits(bv) => sexps.alloc(Sexp::Bits(bv.to_vec())),
            View::Val(v) => match v {
                Val::Bool(true) => sexps.bool_true,
                Val::Bool(false) => sexps.bool_false,
                Val::Bits(bv) => sexps.alloc(Sexp::Bits(bv.to_vec())),
                Val::Symbolic(v) => sexps.alloc(Sexp::Symbolic(*v)),
                Val::Enum(e) => sexps.alloc(Sexp::Enum(e.member, e.enum_id.to_usize())),
                _ => return None,
            },
        })
    }
}

// This type represents the view into an event as we walk down into it.
#[derive(Debug)]
enum EventView<'ev, B> {
    ReadMem { address: &'ev Val<B>, data: &'ev Val<B>, value: &'ev Val<B> },
    WriteMem { address: &'ev Val<B>, data: &'ev Val<B>, value: &'ev Val<B> },
    Abstract { name: String, values: &'ev [Val<B>], return_value: &'ev Val<B> },
    Other { value: View<'ev, B> },
    Default,
}

macro_rules! access_extension {
    ($id: ident, $smt_extension: ident, $concrete_extension: path) => {
        fn $id(&mut self, n: u32, types: &HashMap<Sym, Ty>, sexps: &mut SexpArena) {
            use EventView::*;
            
            if let Some(len) = self.other_sexp_or_bits(types, sexps) {
                if n == len {
                    return;
                } else if n < len {
                    *self = Default;
                    return;
                }
                if let Other { value } = self {
                    match value {
                        View::Sexp(sexp) => {
                            let extend_by = sexps.alloc(Sexp::Int(n - len));
                            let extend = sexps.alloc(Sexp::List(vec![sexps.underscore, sexps.$smt_extension, extend_by]));
                            *self = Other { value: View::Sexp(sexps.alloc(Sexp::List(vec![extend, *sexp]))) }
                        }
                        View::Bits(bv) => {
                            if n > B::MAX_WIDTH {
                                let extend_by = sexps.alloc(Sexp::Int(n - len));
                                let extend = sexps.alloc(Sexp::List(vec![sexps.underscore, sexps.$smt_extension, extend_by]));
                                let sexp = sexps.alloc(Sexp::Bits(bv.to_vec()));
                                *self = Other { value: View::Sexp(sexps.alloc(Sexp::List(vec![extend, sexp]))) }
                            } else {
                                *self = Other { value: View::Bits($concrete_extension(*bv, n)) }
                            }
                        }
                        _ => *self = Default,
                    }
                } else {
                    *self = Default
                }
            } else {
                *self = Default
            }
        }
    }
}

impl<'ev, B: BV> EventView<'ev, B> {
    fn view(&self) -> Option<View<'ev, B>> {
        use EventView::*;
        match self {
            ReadMem { value, .. } => Some(View::Val(value)),
            WriteMem { value, .. } => Some(View::Val(value)),
            Abstract { values, .. } => if values.len() == 1 {
                Some(View::Val(&values[0]))
            } else {
                None
            },
            Other { value } => Some(*value),
            Default => None,
        }
    }
    
    fn other(&mut self) -> &mut Self {
        use EventView::*;
        match self {
            ReadMem { value, .. } => *self = Other { value: View::Val(value) },
            WriteMem { value, .. } => *self = Other { value: View::Val(value) },
            Abstract { values, .. } => if values.len() == 1 {
                *self = Other { value: View::Val(&values[0]) }
            },
            _ => (),
        };
        self
    }

    fn other_sexp_or_bits(&mut self, types: &HashMap<Sym, Ty>, sexps: &mut SexpArena) -> Option<u32> {
        use EventView::*;
        match self.other() {
            Other { value: View::Val(Val::Symbolic(v)) } => {
                if let Some(Ty::BitVec(len)) = types.get(v) {
                    let sexp = sexps.alloc(Sexp::Symbolic(*v));
                    *self = Other { value: View::Sexp(sexp) };
                    Some(*len)
                } else {
                    None
                }
            }
            Other { value: View::Val(Val::Bits(bv)) } => {
                *self = Other { value: View::Bits(*bv) };
                Some(bv.len())
            }
            Other { value: View::Bits(bv) } => {
                Some(bv.len())
            }
            _ => None
        }
    }
    
    fn access_address(&mut self) {
        use EventView::*;
        match self {
            ReadMem { address, .. } => *self = Other { value: View::Val(address) },
            WriteMem { address, .. } => *self = Other { value: View::Val(address) },
            _ => *self = Default,
        }
    }

    fn access_data(&mut self) {
        use EventView::*;
        match self {
            ReadMem { data, .. } => *self = Other { value: View::Val(data) },
            WriteMem { data, .. } => *self = Other { value: View::Val(data) },
            _ => *self = Default,
        }
    }

    fn access_abstract_name(&mut self, expected_name: &str) {
        use EventView::*;
        match self {
            Abstract { name, .. } if name.as_str() == expected_name => *self = Other { value: View::Val(&Val::Bool(true)) },
            _ => *self = Other { value: View::Val(&Val::Bool(false)) },
        }
    }

    fn access_return(&mut self) {
        use EventView::*;
        match self {
            Abstract { return_value, .. } => *self = Other { value: View::Val(return_value) },
            _ => *self = Default,
        }
    }

    fn access_field(&mut self, field: Name, symtab: &Symtab, shared_state: &SharedState<B>) {
        use EventView::*;
        if let Some(sym) = symtab.get(field) {
            if let Other { value: View::Val(Val::Struct(fields)) } = self.other() {
                for (field_name, field_value) in fields {
                    if zencode::decode(shared_state.symtab.to_str_demangled(*field_name)) == sym {
                        *self = Other { value: View::Val(field_value) };
                        return
                    }
                }
            }
        }
        *self = Default
    }

    fn access_tuple(&mut self, n: usize, shared_state: &SharedState<B>) {
        use EventView::*;
        if let Abstract { values, .. } = self {
            *self = Other { value: View::Val(&values[n]) };
            return;
        } else if let Other { value: View::Val(Val::Struct(fields)) } = self.other() {
            for (name, field_value) in fields.iter() {
                if shared_state.symtab.tuple_struct_field_number(*name) == Some(n) {
                    *self = Other { value: View::Val(field_value) };
                    return;
                }
            }
        }
        *self = Default
    }

    fn access_match<'a, 'b, 'c>(&'a mut self, arms: &'b HashMap<Option<Name>, AccessorTree<'c>>, symtab: &Symtab, shared_state: &SharedState<B>) -> &'b AccessorTree<'c> {
        use EventView::*;

        if let Other { value: View::Val(Val::Ctor(ctor_name, value)) } = self.other() {
            let ctor_name = shared_state.symtab.to_str_demangled(*ctor_name);
            *self = Other { value: View::Val(value) };
            let n = &symtab.lookup(&zencode::decode(ctor_name));
            return match arms.get(n) {
                Some(accessor_tree) => accessor_tree,
                // If the constructor isn't in the match arms, return the wildcard using None
                None => &arms[&None],
            }
        };

        *self = Default;
        &ACCESSORTREE_LEAF
    }

    fn access_subvec(&mut self, n: u32, m: u32, types: &HashMap<Sym, Ty>, sexps: &mut SexpArena) {
        use EventView::*;
        
        self.other_sexp_or_bits(types, sexps);
        if let Other { value } = self {
            match value {
                View::Sexp(sexp) => {
                    let n = sexps.alloc(Sexp::Int(n));
                    let m = sexps.alloc(Sexp::Int(m));
                    let extract = sexps.alloc(Sexp::List(vec![sexps.underscore, sexps.extract, n, m]));
                    *self = Other { value: View::Sexp(sexps.alloc(Sexp::List(vec![extract, *sexp]))) }
                }
                View::Bits(bv) => {
                    if let Some(extracted) = bv.extract(n, m) {
                        *self = Other { value: View::Bits(extracted) }
                    } else {
                        *self = Default
                    }
                }
                _ => *self = Default,
            }
        } else {
            *self = Default
        }
    }

    fn access_id(&mut self, id: Name, sexps: &mut SexpArena) {
        use EventView::*;
        
        if id == TRUE.name() {
            *self = Other { value: View::Sexp(sexps.bool_true) }
        } else if id == FALSE.name() {
            *self = Other { value: View::Sexp(sexps.bool_false) }
        } else if id == DEFAULT.name() {
            *self = Default
        }
    }

    access_extension!(access_extz, zero_extend, B::zero_extend);
    access_extension!(access_exts, sign_extend, B::sign_extend);
}

fn generate_ite_chain<'ev, B: BV>(
    event_values: &HashMap<Name, (EventView<'ev, B>, &AccessorTree)>,
    ty: SexpId,
    sexps: &mut SexpArena,
) -> SexpId {
    let mut chain = sexps.alloc_default_value(ty);
    
    for (ev, (event_view, _)) in event_values {
        let result = event_view.view().and_then(|v| v.to_sexp(sexps));
        if let Some(id) = result {
            let ev = sexps.alloc(Sexp::Atom(*ev));
            let comparison = sexps.alloc(Sexp::List(vec![sexps.eq, ev, sexps.ev1]));
            chain = sexps.alloc(Sexp::List(vec![sexps.ite, comparison, id, chain]))
        }
    }

    chain
}

pub fn infer_accessor_type(
    accessors: &[Accessor],
    sexps: &mut SexpArena
) -> SexpId {
    use Accessor::*;

    if let Some(accessor) = accessors.iter().next() {
        match accessor {
            Subvec(hi, lo) => sexps.alloc(Sexp::BitVec((hi - lo) + 1)),
            Extz(n) | Exts(n) => sexps.alloc(Sexp::BitVec(*n)),
            _ => sexps.alloc(Sexp::BitVec(64)),
        }
    } else {
        sexps.alloc(Sexp::BitVec(64))
    }
}

pub fn generate_accessor_function<'ev, B: BV, E: ModelEvent<'ev, B>, V: Borrow<E>>(
    accessor_fn: Name,
    ty: Option<SexpId>,
    accessors: &[Accessor],
    events: &[V],
    types: &HashMap<Sym, Ty>,
    shared_state: &SharedState<B>,
    symtab: &Symtab,
    sexps: &mut SexpArena,
) -> SexpId {
    use Accessor::*;

    let acctree = &AccessorTree::from_accessors(accessors);

    let mut event_values: HashMap<Name, (EventView<'ev, B>, &AccessorTree)> = HashMap::new();

    for event in events {
        let name = event.borrow().name();
        match event.borrow().base() {
            None => {
                event_values.insert(name, (EventView::Default, acctree));
            }
            Some(ev) => match ev {
                Event::ReadMem { address, value, read_kind, .. } => {
                    event_values.insert(name, (EventView::ReadMem { address, data: value, value: read_kind }, acctree));
                }
                Event::WriteMem { address, data, write_kind, .. } => {
                    event_values.insert(name, (EventView::WriteMem { address, data, value: write_kind }, acctree));
                }
                Event::Abstract { name: type_name, primitive, args, return_value } => if *primitive {
                    let type_name = shared_state.symtab.to_str(*type_name).to_string();
                    event_values.insert(name, (EventView::Abstract { name: type_name, values: args, return_value }, acctree));
                }
                Event::ReadReg(_, _, value) => {
                    event_values.insert(name, (EventView::Other { value: View::Val(value) }, acctree));
                }
                Event::WriteReg(_, _, value) => {
                    event_values.insert(name, (EventView::Other { value: View::Val(value) }, acctree));
                }
                _ => (),
            },
        }
    }

    for (view, acctree) in event_values.values_mut() {
        loop {
            log!(log::LITMUS, &format!("{:?}", acctree));
            match acctree {
                AccessorTree::Node { elem, child } => {
                    match *elem {
                        Extz(n) => view.access_extz(*n, types, sexps),
                        Exts(n) => view.access_exts(*n, types, sexps),
                        Subvec(hi, lo) => view.access_subvec(*hi, *lo, types, sexps),
                        Tuple(n) => view.access_tuple(*n, shared_state),
                        Bits(_bitvec) => (),
                        Id(id) => view.access_id(*id, sexps),
                        Field(name) => view.access_field(*name, symtab, shared_state),
                        Length(_n) => (),
                        Address => view.access_address(),
                        Data => view.access_data(),
                        Return => view.access_return(),
                        Is(expected) => view.access_abstract_name(&symtab[*expected]),

                        // Should not occur as an accessortree node
                        Ctor(_) | Wildcard | Match(_) => unreachable!(),
                    }
                    *acctree = child
                }
                AccessorTree::Match { arms } => {
                    let child = view.access_match(arms, symtab, shared_state);
                    *acctree = child
                }
                AccessorTree::Leaf => break,
            }
        }
    }

    let accessor_param = sexps.alloc(Sexp::List(vec![sexps.ev1, sexps.event]));
    let accessor_params = sexps.alloc(Sexp::List(vec![accessor_param]));
    let accessor_ty = match ty {
        Some(ty) => ty,
        None => infer_accessor_type(accessors, sexps),
    };
    let accessor_ite = generate_ite_chain(&event_values, accessor_ty, sexps);
    
    let accessor_fn = sexps.alloc(Sexp::Atom(accessor_fn));
    sexps.alloc(Sexp::List(vec![sexps.define_fun, accessor_fn, accessor_params, accessor_ty, accessor_ite]))
}
