extern crate nix;
extern crate libc;
use std::boxed::Box;
use std::collections;
use std::mem;
use std::sync::Mutex;
use std::sync::atomic;


pub trait EventLoop {
	fn run(&self);
	fn stop(&self);
}


struct EpollHandlerData<'a> {
    on_event: Box<FnMut(nix::sys::epoll::EpollEventKind) + Send + Sync + 'a>,
}


struct WrappedFd {
    pub fd: libc::c_int,
}

impl WrappedFd {
    fn new(fd: libc::c_int) -> WrappedFd {
        WrappedFd {
            fd: fd,
        }
    }
}

impl Drop for WrappedFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}


struct EpollEventLoop<'a> {
	epoll_fd: WrappedFd,
    fd_data: collections::HashMap<libc::c_int, EpollHandlerData<'a>>,
	wakeup_fd: WrappedFd,
	stop: atomic::AtomicBool,
    // Calls to run on wakeup.
    pending_calls: Mutex<collections::VecDeque<Box<FnMut() + Send + 'a>>>,
}


fn only_nix_sys_err<T>(result: nix::NixResult<T>)
        -> Result<T, nix::errno::Errno> {
    match result {
        Ok(v) => Ok(v),
        Err(e) => match e {
            nix::NixError::Sys(errno) => Err(errno),
            _ => panic!(
                "Got a NixError::InvalidPath where I wasn't expecting one."
            ),
        }
    }
}


pub fn new<'a>() -> Result<EpollEventLoop<'a>, nix::errno::Errno> {
	let epoll_fd = WrappedFd::new(try!(
        only_nix_sys_err(nix::sys::epoll::epoll_create())
    ));

    let wakeup_fd = WrappedFd::new(try!(
        only_nix_sys_err(
            nix::sys::eventfd::eventfd(0, nix::sys::eventfd::EFD_NONBLOCK)
        )
    ));

    try!(only_nix_sys_err(nix::sys::epoll::epoll_ctl(
        epoll_fd.fd,
        nix::sys::epoll::EpollOp::EpollCtlAdd,
        wakeup_fd.fd,
        &nix::sys::epoll::EpollEvent {
            events: nix::sys::epoll::EPOLLIN,
            data: wakeup_fd.fd as u64,
        },
    )));

	Ok(EpollEventLoop {
		epoll_fd: epoll_fd,
		wakeup_fd: wakeup_fd,
		stop: atomic::AtomicBool::new(false),
        fd_data: collections::HashMap::new(),
        pending_calls: Mutex::new(collections::VecDeque::new()),
	})
}



impl<'a> EventLoop for EpollEventLoop<'a> {
	fn run(&self) {
		while ! self.should_stop() {
			self.single_loop();
		}
	}
	fn stop(&self) {
        self.stop.store(true, atomic::Ordering::SeqCst);
        self.wakeup();
	}
}

impl<'a> EpollEventLoop<'a> {
    fn should_stop(&self) -> bool {
        self.stop.load(atomic::Ordering::SeqCst)
    }

    fn single_loop(&self) {
        // This gets initialized by the epoll_wait call.
        // Note that only a portion of the array may get initialized.
        let mut events: [nix::sys::epoll::EpollEvent; 16] = unsafe {
            mem::uninitialized()
        };
        let result = only_nix_sys_err(
            nix::sys::epoll::epoll_wait(self.epoll_fd.fd, &mut events, 0)
        );
        let number_of_events: usize;
        match result {
            Ok(size) => number_of_events = size,
            Err(errno) => {
                if errno == nix::errno::EINTR {
                    return
                } else {
                    // Any case not EINTR means something is horribly wrong.
                    panic!("Unknown error after epoll_wait.");
                }
            },
        }

        for index in 0..number_of_events {
            let event = events[index];
            let fd = event.data as libc::c_int;

            if fd == self.wakeup_fd.fd {
                self.handle_wakeup_fd();
            } else {

            }
        }
    }

    fn handle_wakeup_fd(&self) {
        loop {
            let mut this_call;
            // Only pull one at a time out. We release the mutex after each
            // s.t. other threads can pull out other calls.
            {
                let mut pending_calls = self.pending_calls.lock().unwrap();
                match pending_calls.pop_front() {
                    Some(c) => this_call = c,
                    None => {
                        let mut buffer: [u8; 8] = unsafe { mem::uninitialized() };
                        nix::unistd::read(self.wakeup_fd.fd, &mut buffer);
                        break;
                    },
                }
            }
            this_call();
        }
    }

    pub fn call<T: FnMut() + Send + 'a>(&self, func: T) {
        self.pending_calls.lock().unwrap().push_back(Box::new(func));
        self.wakeup();
    }

    fn wakeup(&self) {
        // The eventfd expects us to write 8 bytes representing a u64 in native
        // byte order.
        let one: u64 = 1;
        let buffer: &[u8; 8] = unsafe { mem::transmute(&one) };
        let result = only_nix_sys_err(
            nix::unistd::write(self.wakeup_fd.fd, buffer)
        );
        match result {
            Ok(_) => (),
            Err(errno) => {
                // EAGAIN means the counter is at max, but that means the
                // main loop should be waking soon.
                // Otherwise should never fail.
                if errno != nix::errno::EAGAIN {
                    panic!(
                        "write to wakeup event FD failed: {}",
                        errno.desc(),
                    )
                }
            },
        }
    }
}
