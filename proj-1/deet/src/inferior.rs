use gimli::StableDeref;
use nix::sys::ptrace;
use nix::sys::signal;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::mem::size_of;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;

use crate::dwarf_data::DwarfData;

pub enum Status {
    /// Indicates inferior stopped. Contains the signal that stopped the process, as well as the
    /// current instruction pointer that it is stopped at.
    Stopped(signal::Signal, usize),

    /// Indicates inferior exited normally. Contains the exit status code.
    Exited(i32),

    /// Indicates the inferior exited due to a signal. Contains the signal that killed the
    /// process.
    Signaled(signal::Signal),
}

/// This function calls ptrace with PTRACE_TRACEME to enable debugging on a process. You should use
/// pre_exec with Command to call this in the child process.
fn child_traceme() -> Result<(), std::io::Error> {
    ptrace::traceme().or(Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "ptrace TRACEME failed",
    )))
}

fn align_addr_to_word(addr: usize) -> usize {
    addr & (-(size_of::<usize>() as isize) as usize)
}

pub struct Inferior {
    child: Child,
    pub replaced_values: HashMap<usize, u8>,
}

impl Inferior {
    /// Attempts to start a new inferior process. Returns Some(Inferior) if successful, or None if
    /// an error is encountered.
    pub fn new(target: &str, args: &Vec<String>, breakpoints: &Vec<usize>) -> Option<Inferior> {
        let mut cmd = Command::new(target);
        cmd.args(args);
        unsafe {
            cmd.pre_exec(child_traceme);
        }
        let child = cmd.spawn().expect("fail to spawn target programme");
        let mut inferior = Inferior {
            child,
            replaced_values: HashMap::new(),
        };
        match inferior.wait(None) {
            Ok(status) => match status {
                Status::Exited(exit_code) => {
                    println!("target programme exited prematurely (status {})", exit_code);
                    return None;
                }
                Status::Signaled(signal) => {
                    if signal.eq(&signal::Signal::SIGTRAP) {
                        println!("target programme killed by SIGTRAP");
                        return None;
                    }
                }
                Status::Stopped(signal, _) => {
                    if signal.eq(&signal::Signal::SIGTRAP) {
                        for addr in breakpoints.iter() {
                            // install breakpoints
                            match inferior.write_byte(*addr, 0xcc) {
                                Ok(_) => {}
                                Err(err) => println!(
                                    "failed to set breakpoint at position {:#x}, {}",
                                    *addr, err
                                ),
                            }
                        }
                        return Some(inferior);
                    }
                }
            },
            Err(err) => {
                println!("failed to stop target programme, {}", err);
                return None;
            }
        }
        println!("failed to create inferior");
        None
    }

    /// Returns the pid of this inferior.
    pub fn pid(&self) -> Pid {
        nix::unistd::Pid::from_raw(self.child.id() as i32)
    }

    /// Calls waitpid on this inferior and returns a Status to indicate the state of the process
    /// after the waitpid call.
    pub fn wait(&self, options: Option<WaitPidFlag>) -> Result<Status, nix::Error> {
        Ok(match waitpid(self.pid(), options)? {
            WaitStatus::Exited(_pid, exit_code) => Status::Exited(exit_code),
            WaitStatus::Signaled(_pid, signal, _core_dumped) => Status::Signaled(signal),
            WaitStatus::Stopped(_pid, signal) => {
                let regs = ptrace::getregs(self.pid())?;
                Status::Stopped(signal, regs.rip as usize)
            }
            other => panic!("waitpid returned unexpected status: {:?}", other),
        })
    }

    pub fn cont(&self) -> Result<Status, nix::Error> {
        let _ = ptrace::cont(self.pid(), None)?;
        self.wait(None)
    }

    pub fn terminate(&mut self) -> Result<Status, nix::Error> {
        let _ = self.child.kill();
        self.wait(None)
    }

    pub fn print_backtrace(&self, debug_data: &DwarfData) -> Result<(), nix::Error> {
        let mut rip = ptrace::getregs(self.pid())?.rip as usize;
        let mut rbp = ptrace::getregs(self.pid())?.rbp as usize;
        loop {
            let func = debug_data.get_function_from_addr(rip as usize).unwrap();
            println!(
                "%rip {:#x} {} ({})",
                rip,
                func,
                debug_data.get_line_from_addr(rip).unwrap()
            );
            if func == "main" {
                break;
            }
            rip = ptrace::read(self.pid(), (rbp + 8) as ptrace::AddressType)? as usize;
            rbp = ptrace::read(self.pid(), rbp as ptrace::AddressType)? as usize;
        }
        Ok(())
    }

    pub fn write_byte(&mut self, addr: usize, val: u8) -> Result<u8, nix::Error> {
        let aligned_addr = align_addr_to_word(addr);
        let byte_offset = addr - aligned_addr;
        let word = ptrace::read(self.pid(), aligned_addr as ptrace::AddressType)? as u64;
        let origin_byte = (word >> 8 * byte_offset) & 0xff;
        let masked_word = word & !(0xff << 8 * byte_offset);
        let updated_word = masked_word | ((val as u64) << 8 * byte_offset);
        ptrace::write(
            self.pid(),
            aligned_addr as ptrace::AddressType,
            updated_word as *mut std::ffi::c_void,
        )?;
        if val == 0xcc {
            self.replaced_values.insert(addr, origin_byte as u8);
        }
        Ok(origin_byte as u8)
    }
}
