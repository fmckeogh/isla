// MIT License
//
// Copyright (c) 2019 Alasdair Armstrong
//
// Permission is hereby granted, free of charge, to any person
// obtaining a copy of this software and associated documentation
// files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use, copy,
// modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS
// BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN
// ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::ir::{Symtab, Val, HAVE_EXCEPTION};
use crate::smt::smtlib::*;
use crate::smt::Event::*;
use crate::smt::{Accessor, Event, Trace};
use crate::zencode;

/// `renumber_event` Renumbers all the symbolic variables in an event such that multiple event
/// sequences can have disjoint variable identifiers. It takes two `u32` arguments `i` and `total`,
/// such that `i` is the index of our event sequence in the range `0..(total - 1)` inclusive where
/// `total` is the number of event sequences we want to make disjoint.
#[allow(clippy::unneeded_field_pattern)]
pub fn renumber_event(event: &mut Event, i: u32, total: u32) {
    assert!(i < total);
    use Event::*;
    match event {
        Smt(def) => renumber_def(def, i, total),
        Branch(v, _) | Sleeping(v) => *v = (*v * total) + i,
        ReadReg(_, _, value) | WriteReg(_, _, value) | Instr(value) =>
            renumber_val(value, i, total),
        ReadMem { value, read_kind, address, bytes: _ } => {
            renumber_val(value, i, total);
            renumber_val(read_kind, i, total);
            renumber_val(address, i, total);
        }
        WriteMem { value: v, write_kind, address, data, bytes: _ } => {
            *v = (*v * total) + i;
            renumber_val(write_kind, i, total);
            renumber_val(address, i, total);
            renumber_val(data, i, total);
        }
        Cycle | SleepRequest | WakeupRequest => (),
    }
}

fn renumber_exp(exp: &mut Exp, i: u32, total: u32) {
    exp.modify(
        &(|exp| {
            if let Exp::Var(v) = exp {
                *v = (*v * total) + i
            }
        }),
    )
}

fn renumber_val(val: &mut Val, i: u32, total: u32) {
    use Val::*;
    match val {
        Symbolic(v) => *v = (*v * total) + i,
        I64(_) | I128(_) | Bool(_) | Bits(_) | String(_) | Unit | Ref(_) | Poison => (),
        List(vals) | Vector(vals) => vals.iter_mut().for_each(|val| renumber_val(val, i, total)),
        Struct(fields) => fields.iter_mut().for_each(|(_, val)| renumber_val(val, i, total)),
        Ctor(_, val) => renumber_val(val, i, total),
    }
}

fn renumber_def(def: &mut Def, i: u32, total: u32) {
    use Def::*;
    match def {
        DeclareConst(v, _) => *v = (*v * total) + i,
        DefineConst(v, exp) => {
            *v = (*v * total) + i;
            renumber_exp(exp, i, total)
        }
        Assert(exp) => renumber_exp(exp, i, total),
    }
}

/// `uses_in_exp` counts the number of occurences of each variable in an SMTLIB expression.
fn uses_in_exp(uses: &mut HashMap<u32, u32>, exp: &Exp) {
    use Exp::*;
    match exp {
        Var(v) => {
            uses.insert(*v, uses.get(&v).unwrap_or(&0) + 1);
        }
        Bits(_) | Bits64(_, _) | Bool(_) => (),
        Not(exp) | Bvnot(exp) | Bvneg(exp) | Extract(_, _, exp) | ZeroExtend(_, exp) | SignExtend(_, exp) => {
            uses_in_exp(uses, exp)
        }
        Eq(lhs, rhs)
        | Neq(lhs, rhs)
        | And(lhs, rhs)
        | Or(lhs, rhs)
        | Bvand(lhs, rhs)
        | Bvor(lhs, rhs)
        | Bvxor(lhs, rhs)
        | Bvnand(lhs, rhs)
        | Bvnor(lhs, rhs)
        | Bvxnor(lhs, rhs)
        | Bvadd(lhs, rhs)
        | Bvsub(lhs, rhs)
        | Bvmul(lhs, rhs)
        | Bvudiv(lhs, rhs)
        | Bvsdiv(lhs, rhs)
        | Bvurem(lhs, rhs)
        | Bvsrem(lhs, rhs)
        | Bvsmod(lhs, rhs)
        | Bvult(lhs, rhs)
        | Bvslt(lhs, rhs)
        | Bvule(lhs, rhs)
        | Bvsle(lhs, rhs)
        | Bvuge(lhs, rhs)
        | Bvsge(lhs, rhs)
        | Bvugt(lhs, rhs)
        | Bvsgt(lhs, rhs)
        | Bvshl(lhs, rhs)
        | Bvlshr(lhs, rhs)
        | Bvashr(lhs, rhs)
        | Concat(lhs, rhs) => {
            uses_in_exp(uses, lhs);
            uses_in_exp(uses, rhs)
        }
        Ite(cond, then_exp, else_exp) => {
            uses_in_exp(uses, cond);
            uses_in_exp(uses, then_exp);
            uses_in_exp(uses, else_exp)
        }
    }
}

fn uses_in_value(uses: &mut HashMap<u32, u32>, val: &Val) {
    use Val::*;
    match val {
        Symbolic(v) => {
            uses.insert(*v, uses.get(&v).unwrap_or(&0) + 1);
        }
        I64(_) | I128(_) | Bool(_) | Bits(_) | String(_) | Unit | Ref(_) | Poison => (),
        List(vals) | Vector(vals) => vals.iter().for_each(|val| uses_in_value(uses, val)),
        Struct(fields) => fields.iter().for_each(|(_, val)| uses_in_value(uses, val)),
        Ctor(_, val) => uses_in_value(uses, val),
    }
}

/// The `EventReferences` struct contains for every variable `v` in a
/// trace, the set of all it's immediate dependencies, i.e. all the
/// symbols used to directly define `v`, as computed by `uses_in_exp`.
pub struct EventReferences {
    references: HashMap<u32, HashMap<u32, u32>>,
}

impl EventReferences {
    pub fn from_events(events: &[&Event]) -> Self {
        let mut references = HashMap::new();

        for event in events.iter() {
            if let Smt(Def::DefineConst(id, exp)) = event {
                let mut uses = HashMap::new();
                uses_in_exp(&mut uses, exp);
                references.insert(*id, uses);
            }
        }

        EventReferences { references }
    }

    /// Follow all the dependencies of a symbol in the events,
    /// returning the set symbols it recursively depends on,
    /// (including itself).
    pub fn dependencies(&self, symbol: u32) -> HashSet<u32> {
        let empty_map = HashMap::new();
        // The dependencies for symbol
        let mut deps = HashSet::new();
        deps.insert(symbol);
        // Add symbols to this set once all their immediate
        // dependencies have been added to deps
        let mut seen = HashSet::new();

        loop {
            let mut next = HashSet::new();

            for symbol in deps.iter() {
                if !seen.contains(symbol) {
                    let immediate_deps = self.references.get(&symbol).unwrap_or_else(|| &empty_map);
                    for k in immediate_deps.keys() {
                        next.insert(*k);
                    }
                    seen.insert(*symbol);
                }
            }

            // Terminate when we have no more dependences to add
            if next.is_empty() {
                break;
            } else {
                for symbol in next.iter() {
                    deps.insert(*symbol);
                }
            }
        }

        deps
    }

    /// Returns the set of registers a symbolic variable is tainted
    /// by, i.e. any symbolic registers upon which the variable
    /// depends upon. Also returns whether the value depends upon a
    /// symbolic memory read.
    pub fn taints(&self, symbol: u32, events: &[&Event]) -> (HashSet<u32>, bool) {
        let deps = self.dependencies(symbol);
        let mut taints = HashSet::new();
        let mut memory = false;

        for event in events.iter() {
            match event {
                ReadReg(reg, _, value) => {
                    let mut uses = HashMap::new();
                    uses_in_value(&mut uses, value);
                    for (taint, _) in uses.iter() {
                        if deps.contains(taint) {
                            taints.insert(*reg);
                            break;
                        }
                    }
                }

                ReadMem { value: Val::Symbolic(taint), .. } if deps.contains(taint) => memory = true,

                _ => (),
            }
        }

        (taints, memory)
    }
}

#[allow(clippy::unneeded_field_pattern)]
fn remove_unused_pass<E: Borrow<Event>>(mut events: Vec<E>) -> (Vec<E>, u32) {
    let mut uses: HashMap<u32, u32> = HashMap::new();
    for event in events.iter().rev() {
        use Event::*;
        match event.borrow() {
            Smt(Def::DeclareConst(_, _)) => (),
            Smt(Def::DefineConst(_, exp)) => uses_in_exp(&mut uses, exp),
            Smt(Def::Assert(exp)) => uses_in_exp(&mut uses, exp),
            ReadReg(_, _, val) => uses_in_value(&mut uses, val),
            WriteReg(_, _, val) => uses_in_value(&mut uses, val),
            ReadMem { value: _, read_kind, address, bytes: _ } => {
                uses_in_value(&mut uses, read_kind);
                uses_in_value(&mut uses, address)
            }
            WriteMem { value: _, write_kind, address, data, bytes: _ } => {
                uses_in_value(&mut uses, write_kind);
                uses_in_value(&mut uses, address);
                uses_in_value(&mut uses, data)
            }
            Branch(v, _) => {
                uses.insert(*v, uses.get(&v).unwrap_or(&0) + 1);
            }
            Cycle => (),
            Instr(val) => uses_in_value(&mut uses, val),
            Sleeping(v) => {
                uses.insert(*v, uses.get(&v).unwrap_or(&0) + 1);
            }
            WakeupRequest => (),
            SleepRequest => (),
        }
    }

    let mut removed = 0;

    events.retain(|event| match event.borrow() {
        Smt(Def::DeclareConst(v, _)) => {
            if uses.contains_key(v) {
                true
            } else {
                removed += 1;
                false
            }
        }
        Smt(Def::DefineConst(v, _)) => {
            if uses.contains_key(v) {
                true
            } else {
                removed += 1;
                false
            }
        }
        _ => true,
    });

    (events, removed)
}

pub fn remove_unused<E: Borrow<Event>>(events: Vec<E>) -> Vec<E> {
    let (events, removed) = remove_unused_pass(events);
    if removed > 0 {
        remove_unused(events)
    } else {
        events
    }
}

pub fn simplify(trace: &Trace) -> Vec<&Event> {
    remove_unused(trace.to_vec())
}

fn accessor_to_string(acc: &[Accessor], symtab: &Symtab) -> String {
    acc.iter()
        .map(|elem| elem.to_string(symtab))
        .fold(None, |acc, elem| if let Some(prefix) = acc { Some(format!("{} {}", prefix, elem)) } else { Some(elem) })
        .map_or("nil".to_string(), |acc| format!("({})", acc))
}

// TODO: Handle failure cases better
pub fn write_events_with_opts<B>(events: &[Event], symtab: &Symtab, buf: &mut B, types: bool)
where
    B: fmt::Write,
{
    let mut tcx: HashMap<u32, Ty> = HashMap::new();

    write!(buf, "(trace").unwrap();
    for event in events.iter().rev() {
        (match event {
            Branch(n, loc) => write!(buf, "\n  (branch {} \"{}\")", n, loc),

            Smt(def) if types => {
                write!(buf, "\n  ").unwrap();
                match def {
                    Def::DeclareConst(v, ty) => {
                        tcx.insert(*v, ty.clone());
                        write!(buf, "(declare-const v{} {})", v, ty)
                    }
                    Def::DefineConst(v, exp) => {
                        let ty = exp.infer(&tcx).expect("SMT expression was badly-typed");
                        tcx.insert(*v, ty.clone());
                        write!(buf, "(define-const v{} {} {})", v, ty, exp)
                    }
                    Def::Assert(exp) => write!(buf, "(assert {})", exp),
                }
            }

            Smt(def) => write!(buf, "\n  {}", def),

            ReadMem { value, read_kind, address, bytes } => write!(
                buf,
                "\n  (read-mem {} {} {} {})",
                value.to_string(symtab),
                read_kind.to_string(symtab),
                address.to_string(symtab),
                bytes
            ),

            WriteMem { value, write_kind, address, data, bytes } => write!(
                buf,
                "\n  (write-mem v{} {} {} {} {})",
                value,
                write_kind.to_string(symtab),
                address.to_string(symtab),
                data.to_string(symtab),
                bytes
            ),

            WriteReg(n, acc, v) => write!(
                buf,
                "\n  (write-reg |{}| {} {})",
                zencode::decode(symtab.to_str(*n)),
                accessor_to_string(acc, symtab),
                v.to_string(symtab)
            ),

            ReadReg(n, acc, v) => {
                if *n == HAVE_EXCEPTION {
                    Ok(())
                } else {
                    write!(
                        buf,
                        "\n  (read-reg |{}| {} {})",
                        zencode::decode(symtab.to_str(*n)),
                        accessor_to_string(acc, symtab),
                        v.to_string(symtab)
                    )
                }
            }

            Cycle => write!(buf, "\n  (cycle)"),

            Instr(value) => write!(buf, "\n  (instr {})", value.to_string(symtab)),

            Sleeping(value) => write!(buf, "\n  (sleeping v{})", value),

            SleepRequest => write!(buf, "\n  (sleep-request)"),

            WakeupRequest => write!(buf, "\n  (wake-request)"),
        })
        .expect("Write failed when formatting events")
    }
    writeln!(buf, ")").unwrap();
}

pub fn write_events<B>(events: &[Event], symtab: &Symtab, buf: &mut B)
where
    B: fmt::Write,
{
    write_events_with_opts(events, symtab, buf, false)
}
