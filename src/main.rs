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

#[macro_use]
extern crate lalrpop_util;
#[macro_use]
extern crate lazy_static;

use crossbeam::deque::{Injector, Steal, Stealer, Worker};
use getopts::Options;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::process::exit;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time;
use z3;

mod ast;
mod ast_lexer;
mod concrete;
mod tree;
use ast::*;
use concrete::*;

lalrpop_mod!(#[allow(clippy::all)] pub ast_parser);

struct Trace {
    chunk: Vec<String>,
    next: Option<Arc<Trace>>,
}

type Var = String;

struct Frame {
    pc: usize,
    backjumps: u32,
    vars: Arc<Vec<Var>>,
    instrs: Arc<Vec<Instr<u32>>>,
    stack: Option<Arc<fn(Var, Vec<String>) -> Frame>>,
}

struct LocalFrame {
    pc: usize,
    backjumps: u32,
    vars: Vec<Var>,
    instrs: Arc<Vec<Instr<u32>>>,
    stack: Option<Arc<fn(Var, Vec<String>) -> Frame>>,
    smt: Vec<String>,
}

fn freeze_frame(frame: &LocalFrame) -> Frame {
    Frame {
        pc: frame.pc,
        backjumps: frame.backjumps,
        vars: Arc::new(frame.vars.clone()),
        instrs: frame.instrs.clone(),
        stack: frame.stack.clone(),
    }
}

fn unfreeze_frame(frame: &Frame) -> LocalFrame {
    LocalFrame {
        pc: frame.pc,
        backjumps: frame.backjumps,
        vars: (*frame.vars).clone(),
        instrs: frame.instrs.clone(),
        stack: frame.stack.clone(),
        smt: Vec::new(),
    }
}

fn test_frame() -> Frame {
    Frame {
        pc: 0,
        backjumps: 0,
        vars: Arc::new(Vec::new()),
        instrs: Arc::new(vec![
            Instr::Decl(0, Ty::Bool),
            Instr::Jump(Exp::Id(0), 1),
            Instr::Goto(1),
        ]),
        stack: None,
    }
}

static MAX_BACKJUMPS: u32 = 20;

fn run(queue: &Worker<Frame>, frame: &Frame) -> Result<Vec<String>, String> {
    let mut frame = unfreeze_frame(frame);
    loop {
        if frame.backjumps >= MAX_BACKJUMPS {
            return Err("Too many backwards jumps".to_string());
        }
        match &frame.instrs[frame.pc] {
            Instr::Decl(v, ty) => (),

            Instr::Init(v, ty, exp) => (),

            Instr::Jump(exp, target) => (),

            Instr::Goto(target) => {
                if *target <= frame.pc {
                    frame.backjumps += 1
                }
                frame.pc = *target
            }

            Instr::End => match frame.stack {
                None => return Ok(frame.smt),
                Some(caller) => {
                    frame = unfreeze_frame(&caller(frame.vars[0].clone(), frame.smt.clone()))
                }
            }

            _ => ()
        }
    }
}

fn find_task<T>(
    local: &Worker<T>,
    global: &Injector<T>,
    stealers: &RwLock<Vec<Stealer<T>>>,
) -> Option<T> {
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

fn do_work(queue: &Worker<Frame>, frame: Frame) {
    run(queue, &frame);
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

fn print_usage(opts: Options, code: i32) -> ! {
    let brief = "Usage: isla [options]";
    print!("{}", opts.usage(&brief));
    exit(code)
}

fn load_ir(file: &str) -> std::io::Result<Vec<ast::Def<String>>> {
    let mut file = File::open(file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let lexer = ast_lexer::Lexer::new(&contents);
    match ast_parser::AstParser::new().parse(lexer) {
        Ok(ir) => Ok(ir),
        Err(parse_error) => {
            println!("Parse error: {}", parse_error);
            exit(1)
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut opts = Options::new();
    opts.optopt("t", "threads", "use this many worker threads", "N");
    opts.reqopt("a", "arch", "load architecture file", "FILE");
    opts.optflag("h", "help", "print this help message");
    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            println!("{}", f);
            print_usage(opts, 1)
        }
    };
    if matches.opt_present("h") {
        print_usage(opts, 0)
    }

    let arch = match matches.opt_str("a") {
        Some(file) => match load_ir(&file) {
            Ok(contents) => contents,
            Err(f) => {
                println!("Error when loading architecture: {}", f);
                exit(1)
            }
        },
        None => print_usage(opts, 1),
    };

    let num_threads = match matches.opt_get_default("t", num_cpus::get()) {
        Ok(t) => t,
        Err(f) => {
            println!("Could not parse --threads option: {}", f);
            print_usage(opts, 1)
        }
    };

    let (tx, rx): (SyncSender<Activity>, Receiver<Activity>) = mpsc::sync_channel(2 * num_threads);
    let global: Arc<Injector<Frame>> = Arc::new(Injector::<Frame>::new());
    let stealers: Arc<RwLock<Vec<Stealer<Frame>>>> = Arc::new(RwLock::new(Vec::new()));

    global.push(test_frame());

    let threads: Vec<_> = (0..num_threads)
        .map(|tid| {
            let (poke_tx, poke_rx): (Sender<Response>, Receiver<Response>) = mpsc::channel();
            let thread_tx = tx.clone();
            let global = global.clone();
            let stealers = stealers.clone();

            thread::spawn(move || {
                let q = Worker::new_lifo();
                {
                    let mut stealers = stealers.write().unwrap();
                    stealers.push(q.stealer());
                }
                loop {
                    if let Some(task) = find_task(&q, &global, &stealers) {
                        thread_tx.send(Activity::Busy(tid)).unwrap();
                        do_work(&q, task);
                        while let Some(task) = find_task(&q, &global, &stealers) {
                            do_work(&q, task)
                        }
                    };
                    thread_tx
                        .send(Activity::Idle(tid, poke_tx.clone()))
                        .unwrap();
                    match poke_rx.recv().unwrap() {
                        Response::Poke => (),
                        Response::Kill => break,
                    }
                }
            })
        })
        .collect();

    // Figuring out when to exit is a little complex. We start with
    // only a few threads able to work because we haven't actually
    // explored any of the state space, so all the other workers start
    // idle and repeatedly try to steal work. There may be points when
    // workers have no work, but we want them to become active again
    // if more work becomes available. We therefore want to exit only
    // when 1) all threads are idle, 2) we've told all the threads to
    // steal some work, and 3) all the threads fail to do so and
    // remain idle.
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
                    Activity::Idle(tid, poke) => poke.send(Response::Kill).unwrap(),
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
                Activity::Busy(tid) => (),
            }
        }
        thread::sleep(time::Duration::from_millis(100))
    }

    for child in threads {
        child.join().unwrap()
    }
}
