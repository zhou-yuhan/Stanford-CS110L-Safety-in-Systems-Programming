use std::ops::RangeBounds;

use crate::debugger_command::DebuggerCommand;
use crate::dwarf_data::{DwarfData, Error as DwarfError};
use crate::inferior::Inferior;
use crate::inferior::Status;
use libc::ptrace;
use nix::sys::ptrace;
use rustyline::error::ReadlineError;
use rustyline::Editor;

pub struct Debugger {
    target: String,
    history_path: String,
    readline: Editor<()>,
    inferior: Option<Inferior>,
    debug_data: DwarfData,
    breakpoints: Vec<usize>,
}

impl Debugger {
    /// Initializes the debugger.
    pub fn new(target: &str) -> Debugger {
        let debug_data = match DwarfData::from_file(target) {
            Ok(val) => val,
            Err(DwarfError::ErrorOpeningFile) => {
                println!("could not open file {}", target);
                std::process::exit(1);
            }
            Err(DwarfError::DwarfFormatError(err)) => {
                println!(
                    "could not load debugging symbols from {}: {:?}",
                    target, err
                );
                std::process::exit(1);
            }
        };
        debug_data.print();

        let history_path = format!("{}/.deet_history", std::env::var("HOME").unwrap());
        let mut readline = Editor::<()>::new();
        // Attempt to load history from ~/.deet_history if it exists
        let _ = readline.load_history(&history_path);

        Debugger {
            target: target.to_string(),
            history_path,
            readline,
            inferior: None,
            debug_data,
            breakpoints: Vec::new()
        }
    }

    pub fn parse_addr(&self, addr: &str) -> Option<usize> {
        if addr.to_lowercase().starts_with("0x") {
            // address
            return usize::from_str_radix(&addr[2..], 16).ok();
        } else if String::from(addr).parse::<usize>().is_ok() {
            // line number
            let line_num = String::from(addr).parse::<usize>().expect("can not parse line number");
            return self.debug_data.get_addr_for_line(None, line_num);
        } else {
            // function name
            return self.debug_data.get_addr_for_function(None, addr);
        }
    }

    pub fn print_status(&self, status: Status) {
        match status {
            Status::Exited(exit_code) => {
                println!("target exited (status {})", exit_code);
            }
            Status::Signaled(signal) => {
                println!("target signaled(killed) by {}", signal.as_str());
            }
            Status::Stopped(signal, rip) => {
                println!(
                    "target stopped at {:#x} by signal {} in {} ({})",
                    rip,
                    signal.as_str(),
                    self.debug_data.get_function_from_addr(rip).unwrap(),
                    self.debug_data.get_line_from_addr(rip).unwrap()
                );
            }
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.get_next_command() {
                DebuggerCommand::Run(args) => {
                    // make sure no previous target exists
                    if self.inferior.is_some() {
                        match self.inferior.as_mut().unwrap().terminate() {
                            Ok(status) => self.print_status(status),
                            Err(err) => println!("failed to terminate previous target, {}", err),
                        }
                    }

                    if let Some(inferior) = Inferior::new(&self.target, &args, &self.breakpoints) {
                        // Create the inferior
                        self.inferior = Some(inferior);
                        match self.inferior.as_mut().unwrap().cont() {
                            Ok(status) => self.print_status(status),
                            Err(err) => {
                                println!("failed to run command, {}", err);
                            }
                        }
                    } else {
                        println!("Error starting subprocess");
                    }
                }
                DebuggerCommand::Continue => {
                    if self.inferior.is_none() {
                        println!("please run target first");
                        continue;
                    }
                    // check if inferior is stopped at a breakpoint
                    let inf_ref = self.inferior.as_mut().unwrap();
                    let mut regs = ptrace::getregs(inf_ref.pid()).expect("can not read registers");
                    let rip = regs.rip - 1;
                    if inf_ref.replaced_values.contains_key(&(rip as usize)) {
                        // this is a breakpoint, resume original byte
                        let val = inf_ref.replaced_values.get(&(rip as usize)).unwrap();
                        let trap_byte = inf_ref.write_byte(rip as usize, *val).expect("can not resume original byte");
                        if trap_byte != 0xcc {
                            panic!("failed to resume original byte");
                        }
                        regs.rip = rip;
                        ptrace::setregs(inf_ref.pid(), regs).expect("can not set %rip");

                        // step a intruction and reinstall breakpoint
                        ptrace::step(inf_ref.pid(), None).expect("can not step target");
                        inf_ref.wait(None).expect("can not stop after stepping");
                        inf_ref.write_byte(rip as usize, 0xcc).expect("can not reinstall breakpoint");
                    }

                    match self.inferior.as_mut().unwrap().cont() {
                        Ok(status) => self.print_status(status),
                        Err(err) => {
                            println!("failed to run command, {}", err);
                        }
                    }
                }
                DebuggerCommand::BackTrace => {
                    let _ = self
                        .inferior
                        .as_mut()
                        .unwrap()
                        .print_backtrace(&self.debug_data);
                }
                DebuggerCommand::Breakpoint(s) => {
                    match self.parse_addr(&s) {
                        Some(addr) => {
                            self.breakpoints.push(addr);
                            if self.inferior.is_some() {
                                // inferior is running, add breakpoint
                                match self.inferior.as_mut().unwrap().write_byte(addr, 0xcc) {
                                    Ok(_) => {}
                                    Err(err) => println!(
                                        "failed to set breakpoint at position {:#x}, {}",
                                        addr, err
                                    ),
                                }
                            }
                            println!(
                                "set breakpoint {} at position {:#x}",
                                self.breakpoints.len() - 1,
                                addr
                            );
                        }
                        None => println!("invalid breakpoint format"),
                    };
                }
                DebuggerCommand::Quit => {
                    match self.inferior.as_mut().unwrap().terminate() {
                        Ok(status) => self.print_status(status),
                        Err(err) => {
                            println!("failed to terminate target, {}", err);
                        }
                    }
                    return;
                }
            }
        }
    }

    /// This function prompts the user to enter a command, and continues re-prompting until the user
    /// enters a valid command. It uses DebuggerCommand::from_tokens to do the command parsing.
    ///
    /// You don't need to read, understand, or modify this function.
    fn get_next_command(&mut self) -> DebuggerCommand {
        loop {
            // Print prompt and get next line of user input
            match self.readline.readline("(deet) ") {
                Err(ReadlineError::Interrupted) => {
                    // User pressed ctrl+c. We're going to ignore it
                    println!("Type \"quit\" to exit");
                }
                Err(ReadlineError::Eof) => {
                    // User pressed ctrl+d, which is the equivalent of "quit" for our purposes
                    return DebuggerCommand::Quit;
                }
                Err(err) => {
                    panic!("Unexpected I/O error: {:?}", err);
                }
                Ok(line) => {
                    if line.trim().len() == 0 {
                        continue;
                    }
                    self.readline.add_history_entry(line.as_str());
                    if let Err(err) = self.readline.save_history(&self.history_path) {
                        println!(
                            "Warning: failed to save history file at {}: {}",
                            self.history_path, err
                        );
                    }
                    let tokens: Vec<&str> = line.split_whitespace().collect();
                    if let Some(cmd) = DebuggerCommand::from_tokens(&tokens) {
                        return cmd;
                    } else {
                        println!("Unrecognized command.");
                    }
                }
            }
        }
    }
}
