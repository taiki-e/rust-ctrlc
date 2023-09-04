// Copyright (c) 2017 CtrlC developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use crate::error::Error as CtrlcError;
use rustix::fs::OFlags;
use std::os::fd::BorrowedFd;
use std::os::fd::IntoRawFd;
use std::os::raw::c_int;
use std::os::unix::io::RawFd;

static mut PIPE: (RawFd, RawFd) = (-1, -1);

/// Platform specific error type
pub type Error = rustix::io::Errno;

/// Platform specific signal type
pub type Signal = rustix::process::Signal;

extern "C" fn os_handler(_: c_int) {
    // Assuming this always succeeds. Can't really handle errors in any meaningful way.
    let fd = unsafe { BorrowedFd::borrow_raw(PIPE.1) };
    let _ = rustix::io::write(fd, &[0u8]);
}

// pipe2(2) is not available on macOS, iOS, AIX or Haiku, so we need to use pipe(2) and fcntl(2)
#[inline]
#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "haiku",
    target_os = "aix",
    target_os = "nto",
))]
fn pipe2(flags: OFlags) -> Result<(RawFd, RawFd), Error> {
    use rustix::fs::{fcntl_setfd, fcntl_setfl, FdFlags};

    let pipe = rustix::pipe::pipe()?;

    let mut res = Ok(());

    if flags.contains(OFlags::CLOEXEC) {
        res = res
            .and_then(|_| fcntl_setfd(&pipe.0, FdFlags::CLOEXEC))
            .and_then(|_| fcntl_setfd(&pipe.1, FdFlags::CLOEXEC));
    }

    if flags.contains(OFlags::NONBLOCK) {
        res = res
            .and_then(|_| fcntl_setfl(&pipe.0, OFlags::NONBLOCK))
            .and_then(|_| fcntl_setfl(&pipe.1, OFlags::NONBLOCK));
    }

    match res {
        Ok(_) => Ok((pipe.0.into_raw_fd(), pipe.1.into_raw_fd())),
        Err(e) => Err(e),
    }
}

#[inline]
#[cfg(not(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "haiku",
    target_os = "aix",
    target_os = "nto",
)))]
fn pipe2(flags: OFlags) -> Result<(RawFd, RawFd), Error> {
    let pipe = rustix::pipe::pipe_with(flags)?;
    Ok((pipe.0.into_raw_fd(), pipe.1.into_raw_fd()))
}

/// Register os signal handler.
///
/// Must be called before calling [`block_ctrl_c()`](fn.block_ctrl_c.html)
/// and should only be called once.
///
/// # Errors
/// Will return an error if a system error occurred.
///
#[inline]
pub unsafe fn init_os_handler(overwrite: bool) -> Result<(), Error> {
    PIPE = pipe2(OFlags::CLOEXEC)?;

    let close_pipe = |e: Error| -> Error {
        // Try to close the pipes. close() should not fail,
        // but if it does, there isn't much we can do
        let _ = rustix::io::close(PIPE.1);
        let _ = rustix::io::close(PIPE.0);
        e
    };

    // Make sure we never block on write in the os handler.
    if let Err(e) = rustix::fs::fcntl_setfl(BorrowedFd::borrow_raw(PIPE.1), OFlags::NONBLOCK) {
        return Err(close_pipe(e));
    }

    let handler = signal::SigHandler::Handler(os_handler);
    #[cfg(not(target_os = "nto"))]
    let new_action = signal::SigAction::new(
        handler,
        signal::SaFlags::SA_RESTART,
        signal::SigSet::empty(),
    );
    // SA_RESTART is not supported on QNX Neutrino 7.1 and before
    #[cfg(target_os = "nto")]
    let new_action =
        signal::SigAction::new(handler, signal::SaFlags::empty(), signal::SigSet::empty());

    let sigint_old = match signal::sigaction(rustix::process::Signal::Int, &new_action) {
        Ok(old) => old,
        Err(e) => return Err(close_pipe(e)),
    };
    if !overwrite && sigint_old.handler() != signal::SigHandler::SigDfl {
        signal::sigaction(rustix::process::Signal::Int, &sigint_old).unwrap();
        return Err(close_pipe(rustix::io::Errno::EXIST));
    }

    #[cfg(feature = "termination")]
    {
        let sigterm_old = match signal::sigaction(signal::Signal::SIGTERM, &new_action) {
            Ok(old) => old,
            Err(e) => {
                signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
                return Err(close_pipe(e));
            }
        };
        if !overwrite && sigterm_old.handler() != signal::SigHandler::SigDfl {
            signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
            signal::sigaction(signal::Signal::SIGTERM, &sigterm_old).unwrap();
            return Err(close_pipe(nix::Error::EEXIST));
        }
        let sighup_old = match signal::sigaction(signal::Signal::SIGHUP, &new_action) {
            Ok(old) => old,
            Err(e) => {
                signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
                signal::sigaction(signal::Signal::SIGTERM, &sigterm_old).unwrap();
                return Err(close_pipe(e));
            }
        };
        if !overwrite && sighup_old.handler() != signal::SigHandler::SigDfl {
            signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
            signal::sigaction(signal::Signal::SIGTERM, &sigterm_old).unwrap();
            signal::sigaction(signal::Signal::SIGHUP, &sighup_old).unwrap();
            return Err(close_pipe(nix::Error::EEXIST));
        }
    }

    Ok(())
}

/// Blocks until a Ctrl-C signal is received.
///
/// Must be called after calling [`init_os_handler()`](fn.init_os_handler.html).
///
/// # Errors
/// Will return an error if a system error occurred.
///
#[inline]
pub unsafe fn block_ctrl_c() -> Result<(), CtrlcError> {
    let mut buf = [0u8];

    // TODO: Can we safely convert the pipe fd into a std::io::Read
    // with std::os::unix::io::FromRawFd, this would handle EINTR
    // and everything for us.
    loop {
        match rustix::io::read(BorrowedFd::borrow_raw(PIPE.0), &mut buf[..]) {
            Ok(1) => break,
            Ok(_) => return Err(CtrlcError::System(std::io::ErrorKind::UnexpectedEof.into())),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}
