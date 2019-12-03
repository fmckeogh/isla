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

use crossbeam::deque::{Injector, Steal, Stealer, Worker};
use crossbeam::thread;
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::sleep;
use std::time;

use crate::ast::*;
use crate::concrete::Sbits;
use crate::error::Error;
use crate::log::log_from;
use crate::primop;
use crate::zencode;
use isla_smt::*;

/// Create a Symbolic value of a specified type. Can return a concrete value if the type only
/// permits a single value, such as for the unit type or the zero-length bitvector type (which is
/// ideal because SMT solvers don't allow zero-length bitvectors). Compound types like structs will
/// be a concrete structure with symbolic values for each field. Returns the `NoSymbolicType` error
/// if the type cannot be represented in the SMT solver.
fn symbolic<'a>(ty: &Ty<u32>, shared_state: &SharedState, solver: &mut Solver) -> Result<Val<'a>, Error> {
    let smt_ty = match ty {
        Ty::Unit => return Ok(Val::Unit),
        Ty::Bits(0) => return Ok(Val::Bits(Sbits::new(0, 0))),

        Ty::I64 => smtlib::Ty::BitVec(64),
        Ty::I128 => smtlib::Ty::BitVec(128),
        Ty::Bits(sz) => smtlib::Ty::BitVec(*sz),
        Ty::Bool => smtlib::Ty::Bool,
        Ty::Bit => smtlib::Ty::BitVec(1),

        Ty::Struct(name) => {
            if let Some(field_types) = shared_state.structs.get(name) {
                let field_values: Result<HashMap<u32, Val<'a>>, Error> = field_types
                    .iter()
                    .map(|(f, ty)| match symbolic(ty, shared_state, solver) {
                        Ok(value) => Ok((*f, value)),
                        Err(error) => Err(error),
                    })
                    .collect();
                return Ok(Val::Struct(field_values?));
            } else {
                let name = zencode::decode(shared_state.symtab.to_str(*name));
                return Err(Error::Unreachable(format!("Struct {} does not appear to exist!", name)));
            }
        }

        Ty::Enum(name) => {
            // Currently we represent an enum as a byte with a constraint that it's no larger than
            // the maximum size of the enum, rather than using SMTLIB datatypes. This keeps us fully
            // within the QF_BV theory, and in principle allows using SMT solvers that don't support
            // datatypes.
            use isla_smt::smtlib::*;
            let enum_size = shared_state.enums.get(name).unwrap().len();
            let sym = solver.fresh();
            solver.add(Def::DeclareConst(sym, Ty::BitVec(8)));
            solver.add(Def::Assert(Exp::Bvult(Box::new(Exp::Var(sym)), Box::new(primop::smt_u8(enum_size as u8)))));
            return Ok(Val::Symbolic(sym));
        }

        _ => return Err(Error::NoSymbolicType),
    };

    let sym = solver.fresh();
    solver.add(smtlib::Def::DeclareConst(sym, smt_ty));
    Ok(Val::Symbolic(sym))
}

/// Gets a value from a HashMap. Note that this function is set up to handle the following case:
///
/// ```Sail
/// var x;
/// x = 3;
/// ```
///
/// When we declare a variable it has the value `Uninitialized(ty)` where `ty` is its type. When
/// that variable is first accessed it'll be initialized to a symbolic value in the SMT solver if it
/// is still uninitialized. This means that in the above code, because `x` is immediately assigned
/// the value 3, no interaction with the SMT solver will occur.
fn get_and_initialize<'ast>(
    v: u32,
    vars: &mut HashMap<u32, Val<'ast>>,
    shared_state: &SharedState<'ast>,
    solver: &mut Solver,
) -> Result<Option<Val<'ast>>, Error> {
    Ok(match vars.get(&v) {
        Some(Val::Uninitialized(ty)) => {
            let sym = symbolic(ty, shared_state, solver)?;
            vars.insert(v, sym.clone());
            Some(sym)
        }
        Some(value) => Some(value.clone()),
        None => None,
    })
}

fn get_loc_and_initialize<'ast>(
    loc: &Loc<u32>,
    vars: &mut HashMap<u32, Val<'ast>>,
    globals: &mut HashMap<u32, Val<'ast>>,
    shared_state: &SharedState<'ast>,
    solver: &mut Solver,
) -> Result<Option<Val<'ast>>, Error> {
    Ok(match loc {
        Loc::Id(id) => match get_and_initialize(*id, vars, shared_state, solver)? {
            Some(value) => Some(value),
            None => match get_and_initialize(*id, globals, shared_state, solver)? {
                Some(value) => Some(value),
                None => match shared_state.enum_members.get(id) {
                    Some(position) => Some(Val::Bits(Sbits::from_u8(*position))),
                    None => panic!("Symbol {} ({}) not found", zencode::decode(shared_state.symtab.to_str(*id)), id),
                },
            },
        },

        _ => panic!("Cannot get_loc"),
    })
}

fn eval_exp<'ast>(
    exp: &Exp<u32>,
    vars: &mut HashMap<u32, Val<'ast>>,
    globals: &mut HashMap<u32, Val<'ast>>,
    shared_state: &SharedState<'ast>,
    solver: &mut Solver,
) -> Result<Val<'ast>, Error> {
    use Exp::*;
    Ok(match exp {
        Id(v) => match get_and_initialize(*v, vars, shared_state, solver)? {
            Some(value) => value,
            None => match get_and_initialize(*v, globals, shared_state, solver)? {
                Some(value) => {
                    println!("Register read {} ({})", zencode::decode(shared_state.symtab.to_str(*v)), v);
                    value
                }
                None => match shared_state.enum_members.get(v) {
                    Some(position) => Val::Bits(Sbits::from_u8(*position)),
                    None => panic!("Symbol {} ({}) not found", zencode::decode(shared_state.symtab.to_str(*v)), v),
                },
            },
        },
        I64(i) => Val::I64(*i),
        I128(i) => Val::I128(*i),
        Unit => Val::Unit,
        Bool(b) => Val::Bool(*b),
        Bits(bv) => Val::Bits(*bv),
        String(s) => Val::String(s.clone()),
        Undefined(ty) => symbolic(ty, shared_state, solver)?,
        Call(op, args) => {
            let args: Result<_, _> =
                args.iter().map(|arg| eval_exp(arg, vars, globals, shared_state, solver)).collect();
            let args: Vec<Val<'ast>> = args?;
            match op {
                Op::Gt => primop::op_gt(args[0].clone(), args[1].clone(), solver)?,
                Op::Add => primop::op_add(args[0].clone(), args[1].clone(), solver)?,
                Op::BitToBool => primop::op_bit_to_bool(args[0].clone(), solver)?,
                Op::Bvor => primop::or_bits(args[0].clone(), args[1].clone(), solver)?,
                Op::Bvxor => primop::xor_bits(args[0].clone(), args[1].clone(), solver)?,
                Op::Bvand => primop::and_bits(args[0].clone(), args[1].clone(), solver)?,
                Op::Not => primop::not_bool(args[0].clone(), solver)?,
                Op::Slice(len) => primop::op_slice(args[0].clone(), args[1].clone(), *len, solver)?,
                Op::SetSlice => primop::op_set_slice(args[0].clone(), args[1].clone(), args[2].clone(), solver)?,
                Op::Unsigned(len) => symbolic(&Ty::Bits(*len), shared_state, solver)?,
                _ => {
                    println!("{:?}", op);
                    return Err(Error::Unimplemented);
                }
            }
        }
        Field(exp, field) => {
            if let Val::Struct(struct_value) = eval_exp(exp, vars, globals, shared_state, solver)? {
                match struct_value.get(field) {
                    Some(field_value) => field_value.clone(),
                    None => panic!("No field"),
                }
            } else {
                panic!("Struct expression did not evaluate to a struct")
            }
        }
        _ => panic!("Could not evaluate expression {:?}", exp),
    })
}

fn assign<'ast>(
    loc: &Loc<u32>,
    v: Val<'ast>,
    vars: &mut HashMap<u32, Val<'ast>>,
    globals: &mut HashMap<u32, Val<'ast>>,
    shared_state: &SharedState<'ast>,
    solver: &mut Solver,
) -> Result<(), Error> {
    match loc {
        Loc::Id(id) => {
            if vars.contains_key(id) || *id == RETURN {
                vars.insert(*id, v);
            } else {
                globals.insert(*id, v);
            }
        }

        Loc::Field(loc, field) => {
            if let Some(Val::Struct(field_values)) = get_loc_and_initialize(loc, vars, globals, shared_state, solver)? {
                // As a sanity test, check that the field exists.
                match field_values.get(field) {
                    Some(_) => {
                        let mut field_values = field_values.clone();
                        field_values.insert(*field, v);
                        assign(loc, Val::Struct(field_values), vars, globals, shared_state, solver)?;
                    }
                    None => panic!("Invalid field assignment"),
                }
            } else {
                panic!(
                    "Cannot assign struct to non-struct {:?}.{:?} ({:?})",
                    loc,
                    field,
                    get_loc_and_initialize(loc, vars, globals, shared_state, solver)
                )
            }
        }

        _ => panic!("Bad assign"),
    };
    Ok(())
}

/// The callstack is implemented as a closure that restores the caller's stack frame. It
/// additionally takes the shared state asinput also to avoid ownership issues when creating the
/// closure.
#[rustfmt::skip]
type Stack<'ast> =
    Option<Arc<dyn 'ast + Send + Sync + Fn(Val<'ast>, &mut LocalFrame<'ast>, &SharedState<'ast>, &mut Solver) -> Result<(), Error>>>;

pub struct Frame<'ast> {
    pc: usize,
    backjumps: u32,
    vars: Arc<HashMap<u32, Val<'ast>>>,
    globals: Arc<HashMap<u32, Val<'ast>>>,
    instrs: &'ast [Instr<u32>],
    stack: Stack<'ast>,
}

impl<'ast> Frame<'ast> {
    pub fn new(
        args: &[(u32, &'ast Ty<u32>)],
        mut registers: HashMap<u32, Val<'ast>>,
        instrs: &'ast [Instr<u32>],
    ) -> Self {
        let mut vars = HashMap::new();
        for (id, ty) in args {
            vars.insert(*id, Val::Uninitialized(ty));
        }
        registers.insert(HAVE_EXCEPTION, Val::Bool(false));
        Frame { pc: 0, backjumps: 0, vars: Arc::new(vars), globals: Arc::new(registers), instrs, stack: None }
    }
}

pub struct LocalFrame<'ast> {
    pc: usize,
    backjumps: u32,
    pub vars: HashMap<u32, Val<'ast>>,
    pub globals: HashMap<u32, Val<'ast>>,
    instrs: &'ast [Instr<u32>],
    stack: Stack<'ast>,
}

fn unfreeze_frame<'ast>(frame: &Frame<'ast>) -> LocalFrame<'ast> {
    LocalFrame {
        pc: frame.pc,
        backjumps: frame.backjumps,
        vars: (*frame.vars).clone(),
        globals: (*frame.globals).clone(),
        instrs: frame.instrs,
        stack: frame.stack.clone(),
    }
}

fn freeze_frame<'ast>(frame: &LocalFrame<'ast>) -> Frame<'ast> {
    Frame {
        pc: frame.pc,
        backjumps: frame.backjumps,
        vars: Arc::new(frame.vars.clone()),
        globals: Arc::new(frame.globals.clone()),
        instrs: frame.instrs,
        stack: frame.stack.clone(),
    }
}

fn run<'ast>(
    tid: usize,
    queue: &Worker<(Frame<'ast>, Checkpoint)>,
    frame: &Frame<'ast>,
    shared_state: &SharedState<'ast>,
    solver: &mut Solver,
) -> Result<(Val<'ast>, LocalFrame<'ast>), Error> {
    let mut frame = unfreeze_frame(frame);
    loop {
        if frame.pc >= frame.instrs.len() {
            return Ok((Val::Unit, frame));
        }
        match &frame.instrs[frame.pc] {
            Instr::Decl(v, ty) => {
                frame.vars.insert(*v, Val::Uninitialized(ty));
                frame.pc += 1;
            }

            Instr::Init(var, _, exp) => {
                let value = eval_exp(exp, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                frame.vars.insert(*var, value);
                frame.pc += 1;
            }

            Instr::Jump(exp, target) => {
                let value = eval_exp(exp, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                match value {
                    Val::Symbolic(v) => {
                        use smtlib::Def::*;
                        use smtlib::Exp::*;
                        let test_true = Var(v);
                        let test_false = Not(Box::new(Var(v)));
                        let can_be_true = solver.check_sat_with(&test_true).is_sat();
                        let can_be_false = solver.check_sat_with(&test_false).is_sat();
                        if can_be_true && can_be_false {
                            let point = solver.checkpoint_with(Assert(test_false));
                            let frozen = Frame { pc: frame.pc + 1, ..freeze_frame(&frame) };
                            log_from(tid, 0, &format!("Choice @ {}", frame.pc));
                            queue.push((frozen, point));
                            solver.add(Assert(test_true));
                            frame.pc = *target
                        } else if can_be_true {
                            log_from(tid, 0, &format!("True @ {}", frame.pc));
                            solver.add(Assert(test_true));
                            frame.pc = *target
                        } else if can_be_false {
                            log_from(tid, 0, &format!("False @ {}", frame.pc));
                            solver.add(Assert(test_false));
                            frame.pc += 1
                        } else {
                            // This execution path is dead
                            return Err(Error::Dead);
                        }
                    }
                    Val::Bool(jump) => {
                        if jump {
                            frame.pc = *target
                        } else {
                            frame.pc += 1
                        }
                    }
                    _ => {
                        return Err(Error::Type("Jump on non boolean"));
                    }
                }
            }

            Instr::Goto(target) => frame.pc = *target,

            Instr::Copy(loc, exp) => {
                let value = eval_exp(exp, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                assign(loc, value, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                frame.pc += 1;
            }

            Instr::PrimopUnary(loc, f, arg) => {
                let arg = eval_exp(arg, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                let value = f(arg, solver)?;
                assign(loc, value, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                frame.pc += 1;
            }

            Instr::PrimopBinary(loc, f, arg1, arg2) => {
                let arg1 = eval_exp(arg1, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                let arg2 = eval_exp(arg2, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                let value = f(arg1, arg2, solver)?;
                assign(loc, value, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                frame.pc += 1;
            }

            Instr::PrimopVariadic(loc, f, args) => {
                let args: Result<_, _> = args
                    .iter()
                    .map(|arg| eval_exp(arg, &mut frame.vars, &mut frame.globals, shared_state, solver))
                    .collect();
                let value = f(args?, solver)?;
                assign(loc, value, &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                frame.pc += 1;
            }

            Instr::Call(loc, _, f, args) => {
                match shared_state.functions.get(&f) {
                    None => {
                        if *f == INTERNAL_VECTOR_INIT && args.len() == 1 {
                            let arg = eval_exp(&args[0], &mut frame.vars, &mut frame.globals, shared_state, solver)?;
                            match loc {
                                Loc::Id(v) => match (arg, frame.vars.get(v)) {
                                    (Val::I64(len), Some(Val::Uninitialized(Ty::Vector(ty)))) => assign(
                                        loc,
                                        Val::Vector(vec![Val::Uninitialized(ty); len as usize]),
                                        &mut frame.vars,
                                        &mut frame.globals,
                                        shared_state,
                                        solver,
                                    )?,
                                    _ => return Err(Error::Type("internal_vector_init")),
                                },
                                _ => return Err(Error::Type("internal_vector_init")),
                            };
                            frame.pc += 1
                        } else if *f == INTERNAL_VECTOR_UPDATE {
                            frame.pc += 1
                        } else if *f == SAIL_EXIT {
                            return Err(Error::Exit);
                        } else {
                            let symbol = zencode::decode(shared_state.symtab.to_str(*f));
                            panic!("Attempted to call non-existent function {} ({})", symbol, *f)
                        }
                    }

                    Some((params, _, instrs)) => {
                        let symbol = zencode::decode(shared_state.symtab.to_str(*f));
                        log_from(tid, 0, &format!("Calling {}", symbol));
                        let args: Result<Vec<Val<'ast>>, _> = args
                            .iter()
                            .map(|arg| eval_exp(arg, &mut frame.vars, &mut frame.globals, shared_state, solver))
                            .collect();
                        let caller = freeze_frame(&frame);
                        // Set up a closure to restore our state when
                        // the function we call returns
                        frame.stack = Some(Arc::new(move |ret, frame, shared_state, solver| {
                            frame.pc = caller.pc + 1;
                            frame.vars = (*caller.vars).clone();
                            frame.instrs = caller.instrs;
                            frame.stack = caller.stack.clone();
                            assign(loc, ret, &mut frame.vars, &mut frame.globals, shared_state, solver)
                        }));
                        frame.vars.clear();
                        for (i, arg) in args?.drain(..).enumerate() {
                            frame.vars.insert(params[i].0, arg);
                        }
                        frame.pc = 0;
                        frame.instrs = instrs;
                    }
                }
            }

            Instr::End => match frame.vars.get(&RETURN) {
                None => panic!("Reached end without assigning to return"),
                Some(value) => {
                    let caller = match &frame.stack {
                        None => return Ok((value.clone(), frame)),
                        Some(caller) => caller.clone(),
                    };
                    (*caller)(value.clone(), &mut frame, shared_state, solver)?
                }
            },

            _ => frame.pc += 1,
        }
    }
}

/// A collector is run on the result of each path found via symbolic execution through the code. It
/// takes the result of the execution, which is either a combination of the return value and local
/// state at the end of the execution or an error, as well as the shared state and the SMT solver
/// state associated with that execution. It build a final result for all the executions by
/// collecting the results into a type R, protected by a lock.
type Collector<'ast, R> = dyn 'ast
    + Sync
    + Fn(usize, Result<(Val<'ast>, LocalFrame<'ast>), Error>, &SharedState<'ast>, &mut Solver, &Mutex<R>) -> ();

type Task<'ast> = (Frame<'ast>, Checkpoint);

/// Start symbolically executing a Task using just the current thread, collecting the results using
/// the given collector.
pub fn start_single<'ast, R>(
    (frame, checkpoint): Task<'ast>,
    shared_state: &SharedState<'ast>,
    collected: &Mutex<R>,
    collector: &Collector<'ast, R>,
) {
    let queue = Worker::new_lifo();
    queue.push((frame, checkpoint));
    while let Some((frame, checkpoint)) = queue.pop() {
        let cfg = Config::new();
        let ctx = Context::new(cfg);
        let mut solver = Solver::from_checkpoint(&ctx, checkpoint);
        let result = run(0, &queue, &frame, shared_state, &mut solver);
        collector(0, result, shared_state, &mut solver, collected)
    }
}

fn find_task<T>(local: &Worker<T>, global: &Injector<T>, stealers: &RwLock<Vec<Stealer<T>>>) -> Option<T> {
    let stealers = stealers.read().unwrap();
    local.pop().or_else(|| {
        std::iter::repeat_with(|| {
            let stolen: Steal<T> = stealers.iter().map(|s| s.steal()).collect();
            stolen.or_else(|| global.steal_batch_and_pop(local))
        })
        .find(|s| !s.is_retry())
        .and_then(|s| s.success())
    })
}

fn do_work<'ast, R>(
    tid: usize,
    queue: &Worker<Task<'ast>>,
    (frame, checkpoint): Task<'ast>,
    shared_state: &SharedState<'ast>,
    collected: &Mutex<R>,
    collector: &Collector<'ast, R>,
) {
    let cfg = Config::new();
    let ctx = Context::new(cfg);
    let mut solver = Solver::from_checkpoint(&ctx, checkpoint);
    let result = run(tid, queue, &frame, shared_state, &mut solver);
    collector(tid, result, shared_state, &mut solver, collected)
}

enum Response {
    Poke,
    Kill,
}

#[derive(Clone)]
enum Activity {
    Idle(usize, Sender<Response>),
    Busy(usize),
}

/// Start symbolically executing a Task across `num_threads` new threads, collecting the results
/// using the given collector.
pub fn start_multi<'ast, R>(
    num_threads: usize,
    task: Task<'ast>,
    shared_state: &SharedState<'ast>,
    collected: Arc<Mutex<R>>,
    collector: &Collector<'ast, R>,
) where
    R: Send,
{
    let (tx, rx): (SyncSender<Activity>, Receiver<Activity>) = mpsc::sync_channel(2 * num_threads);
    let global: Arc<Injector<Task>> = Arc::new(Injector::<Task>::new());
    let stealers: Arc<RwLock<Vec<Stealer<Task>>>> = Arc::new(RwLock::new(Vec::new()));

    global.push(task);

    thread::scope(|scope| {
        for tid in 0..num_threads {
            // When a worker is idle, it reports that to the main orchestrating thread, which can
            // then 'poke' it to wait it up via a channel, which will cause the worker to try to
            // steal some work, or the main thread can kill the worker.
            let (poke_tx, poke_rx): (Sender<Response>, Receiver<Response>) = mpsc::channel();
            let thread_tx = tx.clone();
            let global = global.clone();
            let stealers = stealers.clone();
            let collected = collected.clone();

            scope.spawn(move |_| {
                let q = Worker::new_lifo();
                {
                    let mut stealers = stealers.write().unwrap();
                    stealers.push(q.stealer());
                }
                loop {
                    if let Some(task) = find_task(&q, &global, &stealers) {
                        log_from(tid, 0, "Working");
                        thread_tx.send(Activity::Busy(tid)).unwrap();
                        do_work(tid, &q, task, &shared_state, &collected, collector);
                        while let Some(task) = find_task(&q, &global, &stealers) {
                            do_work(tid, &q, task, &shared_state, &collected, collector)
                        }
                    };
                    log_from(tid, 0, "Idle");
                    thread_tx.send(Activity::Idle(tid, poke_tx.clone())).unwrap();
                    match poke_rx.recv().unwrap() {
                        Response::Poke => (),
                        Response::Kill => break,
                    }
                }
            });
        }

        // Figuring out when to exit is a little complex. We start with only a few threads able to
        // work because we haven't actually explored any of the state space, so all the other
        // workers start idle and repeatedly try to steal work. There may be points when workers
        // have no work, but we want them to become active again if more work becomes available. We
        // therefore want to exit only when 1) all threads are idle, 2) we've told all the threads
        // to steal some work, and 3) all the threads fail to do so and remain idle.
        let mut current_activity = vec![0; num_threads];
        let mut last_messages = vec![Activity::Busy(0); num_threads];
        loop {
            loop {
                match rx.try_recv() {
                    Ok(Activity::Busy(tid)) => {
                        last_messages[tid] = Activity::Busy(tid);
                        current_activity[tid] = 0;
                    }
                    Ok(Activity::Idle(tid, poke)) => {
                        last_messages[tid] = Activity::Idle(tid, poke);
                        current_activity[tid] += 1;
                    }
                    Err(_) => break,
                }
            }
            let mut quiescent = true;
            for idleness in &current_activity {
                if *idleness < 2 {
                    quiescent = false
                }
            }
            if quiescent {
                for message in &last_messages {
                    match message {
                        Activity::Idle(_tid, poke) => poke.send(Response::Kill).unwrap(),
                        Activity::Busy(tid) => panic!("Found busy thread {} when quiescent", tid),
                    }
                }
                break;
            }
            for message in &last_messages {
                match message {
                    Activity::Idle(tid, poke) => {
                        poke.send(Response::Poke).unwrap();
                        current_activity[*tid] = 1;
                    }
                    Activity::Busy(_) => (),
                }
            }
            sleep(time::Duration::from_millis(100))
        }
    })
    .unwrap();
}

pub fn all_unsat_collector<'ast>(
    tid: usize,
    result: Result<(Val<'ast>, LocalFrame<'ast>), Error>,
    _shared_state: &SharedState<'ast>,
    solver: &mut Solver,
    collected: &Mutex<bool>,
) {
    match result {
        Ok(value) => match value {
            (Val::Symbolic(v), _) => {
                use smtlib::Def::*;
                use smtlib::Exp::*;
                solver.add(Assert(Not(Box::new(Var(v)))));
                if solver.check_sat() != SmtResult::Unsat {
                    log_from(tid, 0, "Got sat");
                    let mut b = collected.lock().unwrap();
                    *b &= false
                } else {
                    log_from(tid, 0, "Got unsat")
                }
            }
            (Val::Bool(true), _) => log_from(tid, 0, "Got true"),
            (Val::Bool(false), _) => {
                log_from(tid, 0, "Got false");
                let mut b = collected.lock().unwrap();
                *b &= false
            }
            (value, _) => log_from(tid, 0, &format!("Got value {:?}", value)),
        },
        Err(err) => match err {
            Error::Dead => (),
            _ => {
                log_from(tid, 0, &format!("Got error, {:?}", err));
                let mut b = collected.lock().unwrap();
                *b &= false
            }
        },
    }
}