use gimli::StableDeref;
use nix::sys::ptrace;
use nix::sys::signal;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;

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

pub struct Inferior {
    child: Child,
}

impl Inferior {
    /// Attempts to start a new inferior process. Returns Some(Inferior) if successful, or None if
    /// an error is encountered.
    pub fn new(target: &str, args: &Vec<String>) -> Option<Inferior> {
        let mut cmd = Command::new(target);
        cmd.args(args);
        unsafe {
            cmd.pre_exec(child_traceme);
        }
        let child = cmd.spawn().expect("fail to spawn target programme");
        let inferior = Inferior { child };
        match inferior.wait(None) {
            Ok(status) => match status {
                Status::Exited(exit_code) => {
                    println!("taget programme exited prematurely (status {})", exit_code);
                    return None;
                }
                Status::Signaled(signal) => {
                    if signal.eq(&signal::Signal::SIGTRAP) {
                        println!("target programme killed by SIGTRAP");
                        return Some(inferior);
                    }
                }
                Status::Stopped(signal, _) => {
                    if signal.eq(&signal::Signal::SIGTRAP) {
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
}
