use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;
use uapi::c;

const TCIFLUSH: c::c_int = 0;
const IORING_OP_READ: u8 = 22;
const IORING_OP_LINK_TIMEOUT: u8 = 15;
const IOSQE_IO_LINK: u8 = 1 << 2;
const USER_DATA_READ: u64 = 0x1623;
const USER_DATA_TIMEOUT: u64 = 0x1624;
const IORING_OFF_SQ_RING: c::off_t = 0;
const IORING_OFF_CQ_RING: c::off_t = 0x800_0000;
const IORING_OFF_SQES: c::off_t = 0x1000_0000;
const IORING_FEAT_SINGLE_MMAP: u32 = 1;
const RING_ENTRIES: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    rw_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    pad2: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[derive(Clone, Copy)]
struct CqPtrs {
    head: usize,
    tail: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqringOffsets,
    cq_off: CqringOffsets,
}

fn map(len: usize, fd: c::c_int, off: c::off_t) -> *mut u8 {
    let p = unsafe {
        c::mmap(
            std::ptr::null_mut(),
            len,
            c::PROT_READ | c::PROT_WRITE,
            c::MAP_SHARED | c::MAP_POPULATE,
            fd,
            off,
        )
    };
    if p == c::MAP_FAILED {
        eprintln!("mmap failed: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    p as *mut u8
}

struct Ring {
    fd: c::c_int,
    sq_tail: *mut u32,
    sq_mask: u32,
    sq_array: *mut u32,
    sqes: *mut Sqe,
    cq_head: *mut u32,
    cq_tail: *mut u32,
    cq_mask: u32,
    cqes: *mut Cqe,
}

impl Ring {
    fn new() -> Ring {
        let mut p = Params::default();
        let fd = unsafe {
            c::syscall(
                c::SYS_io_uring_setup,
                RING_ENTRIES as usize,
                &mut p as *mut Params as usize,
            )
        } as c::c_int;
        if fd < 0 {
            eprintln!("io_uring_setup: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        let sq_len = p.sq_off.array as usize + p.sq_entries as usize * 4;
        let cq_len = p.cq_off.cqes as usize + p.cq_entries as usize * std::mem::size_of::<Cqe>();
        let (sq, cq) = if p.features & IORING_FEAT_SINGLE_MMAP != 0 {
            let ring = map(sq_len.max(cq_len), fd, IORING_OFF_SQ_RING);
            (ring, ring)
        } else {
            (
                map(sq_len, fd, IORING_OFF_SQ_RING),
                map(cq_len, fd, IORING_OFF_CQ_RING),
            )
        };
        let sqes = map(
            p.sq_entries as usize * std::mem::size_of::<Sqe>(),
            fd,
            IORING_OFF_SQES,
        );

        unsafe {
            Ring {
                fd,
                sq_tail: sq.add(p.sq_off.tail as usize) as *mut u32,
                sq_mask: *(sq.add(p.sq_off.ring_mask as usize) as *const u32),
                sq_array: sq.add(p.sq_off.array as usize) as *mut u32,
                sqes: sqes as *mut Sqe,
                cq_head: cq.add(p.cq_off.head as usize) as *mut u32,
                cq_tail: cq.add(p.cq_off.tail as usize) as *mut u32,
                cq_mask: *(cq.add(p.cq_off.ring_mask as usize) as *const u32),
                cqes: cq.add(p.cq_off.cqes as usize) as *mut Cqe,
            }
        }
    }

    fn submit_read_with_timeout(
        &self,
        fd: c::c_int,
        addr: *mut u8,
        len: u32,
        timeout: *const Timespec,
    ) {
        unsafe {
            let tail = std::ptr::read_volatile(self.sq_tail);

            let idx = tail & self.sq_mask;
            let sqe = &mut *self.sqes.add(idx as usize);
            *sqe = Sqe::default();
            sqe.opcode = IORING_OP_READ;
            sqe.flags = IOSQE_IO_LINK;
            sqe.fd = fd;
            sqe.off = u64::MAX;
            sqe.addr = addr as u64;
            sqe.len = len;
            sqe.user_data = USER_DATA_READ;
            std::ptr::write_volatile(self.sq_array.add(idx as usize), idx);

            let idx = tail.wrapping_add(1) & self.sq_mask;
            let sqe = &mut *self.sqes.add(idx as usize);
            *sqe = Sqe::default();
            sqe.opcode = IORING_OP_LINK_TIMEOUT;
            sqe.addr = timeout as u64;
            sqe.len = 1;
            sqe.user_data = USER_DATA_TIMEOUT;
            std::ptr::write_volatile(self.sq_array.add(idx as usize), idx);

            std::sync::atomic::fence(Ordering::Release);
            std::ptr::write_volatile(self.sq_tail, tail.wrapping_add(2));
        }
    }

    fn enter(&self, to_submit: u32) -> c::c_int {
        unsafe {
            c::syscall(
                c::SYS_io_uring_enter,
                self.fd as usize,
                to_submit as usize,
                0usize,
                0usize,
                0usize,
                0usize,
            ) as c::c_int
        }
    }

    fn peek_cqe(&self) -> Option<Cqe> {
        unsafe {
            let head = std::ptr::read_volatile(self.cq_head);
            let tail = std::ptr::read_volatile(self.cq_tail);
            if head == tail {
                return None;
            }
            let cqe = std::ptr::read_volatile(self.cqes.add((head & self.cq_mask) as usize));
            std::ptr::write_volatile(self.cq_head, head.wrapping_add(1));
            Some(cqe)
        }
    }
}

fn pin_current_cpu() {
    let mut mask = [0usize; 16];
    if uapi::sched_getaffinity(0, &mut mask).is_err() {
        return;
    }
    let Some((word, bits)) = mask.iter().enumerate().find(|(_, m)| **m != 0) else {
        return;
    };
    let cpu = word * usize::BITS as usize + bits.trailing_zeros() as usize;
    let mut m = [0usize; 16];
    m[cpu / usize::BITS as usize] = 1usize << (cpu % usize::BITS as usize);
    let _ = uapi::sched_setaffinity(0, &m[..cpu / usize::BITS as usize + 1]);
}

fn wchan(tid: c::pid_t) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/wchan"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

fn syscall_of(tid: c::pid_t) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall"))
        .map(|s| s.split_whitespace().next().unwrap_or("?").to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

fn stack_trace(tid: c::pid_t) -> String {
    match std::fs::read_to_string(format!("/proc/self/task/{tid}/stack")) {
        Ok(s) => s.trim_end().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            "<denied: /proc/<tid>/stack needs CAP_SYS_ADMIN; re-run with sudo>".to_string()
        }
        Err(e) => format!("<unavailable: {e}>"),
    }
}

fn fd_flags(fd: c::c_int) -> c::c_int {
    unsafe { c::fcntl(fd, c::F_GETFL) }
}

fn create_pty() -> (uapi::OwnedFd, uapi::OwnedFd) {
    let master = uapi::open("/dev/ptmx", c::O_RDWR | c::O_NOCTTY, 0).expect("open /dev/ptmx");
    let mut unlock: c::c_int = 0;
    let r = unsafe { c::ioctl(master.as_raw_fd(), c::TIOCSPTLCK, &mut unlock) };
    assert_eq!(r, 0, "TIOCSPTLCK: {}", std::io::Error::last_os_error());
    let mut ptn: c::c_int = 0;
    let r = unsafe { c::ioctl(master.as_raw_fd(), c::TIOCGPTN, &mut ptn) };
    assert_eq!(r, 0, "TIOCGPTN: {}", std::io::Error::last_os_error());
    let path = format!("/dev/pts/{ptn}");
    let slave = uapi::open(path.as_str(), c::O_RDWR | c::O_NOCTTY, 0).expect("open pts");
    (master, slave)
}

fn spawn_consumer(master: c::c_int, tid_slot: Arc<AtomicI32>) -> thread::JoinHandle<c::ssize_t> {
    thread::spawn(move || {
        let _ = uapi::setpriority(c::PRIO_PROCESS as c::c_int, 0, 19);
        pin_current_cpu();
        tid_slot.store(uapi::gettid(), Ordering::SeqCst);
        let mut b = [0u8; 1];
        unsafe { c::read(master, b.as_mut_ptr() as *mut c::c_void, 1) }
    })
}

fn spawn_watchdog(
    attempt: u32,
    main_tid: c::pid_t,
    consumer_tid: c::pid_t,
    slave_fd: c::c_int,
    cq: CqPtrs,
    done: Arc<AtomicBool>,
    hung: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(100));
            if done.load(Ordering::SeqCst) {
                return;
            }
        }
        hung.store(true, Ordering::SeqCst);
        let completions = unsafe {
            let head = std::ptr::read_volatile(cq.head as *const u32);
            let tail = std::ptr::read_volatile(cq.tail as *const u32);
            tail.wrapping_sub(head)
        };
        println!("[{attempt}] HANG: io_uring_enter has not returned for 3s");
        println!(
            "[{attempt}] 100ms link timeout did not fire: {completions} CQEs after 3s (read={USER_DATA_READ:#x}, timeout={USER_DATA_TIMEOUT:#x})"
        );
        println!(
            "[{attempt}] submitter tid {main_tid} in syscall {} (426=io_uring_enter), wchan: {}",
            syscall_of(main_tid),
            wchan(main_tid)
        );
        println!(
            "[{attempt}] consumer  tid {consumer_tid} wchan: {}",
            wchan(consumer_tid)
        );
        println!(
            "[{attempt}] submitter kernel stack:\n{}",
            stack_trace(main_tid)
        );
        println!(
            "[{attempt}] consumer kernel stack:\n{}",
            stack_trace(consumer_tid)
        );
        let byte = b"x";
        let r = unsafe { c::write(slave_fd, byte.as_ptr() as *const c::c_void, 1) };
        println!("[{attempt}] fed 1 byte into the slave (write={r})");
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(100));
            if done.load(Ordering::SeqCst) {
                println!("[{attempt}] io_uring_enter returned after input was fed");
                println!("[{attempt}] TRIGGERED: io_uring_enter was blocked inside the tty read");
                std::process::exit(0);
            }
        }
        println!("[{attempt}] still blocked after feeding input");
        std::process::exit(2);
    });
}

fn run_attempt(attempt: u32, with_consumer: bool) -> bool {
    let (master, slave) = create_pty();

    let one: c::c_int = 1;
    let r = unsafe { c::ioctl(master.as_raw_fd(), c::TIOCPKT, &one) };
    assert_eq!(r, 0, "TIOCPKT: {}", std::io::Error::last_os_error());

    let flags = fd_flags(master.as_raw_fd());
    println!(
        "[{attempt}] master flags {flags:#x} (O_NONBLOCK={})",
        flags & c::O_NONBLOCK != 0
    );

    let ring = Ring::new();
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(AtomicBool::new(false));
    let tid_slot = Arc::new(AtomicI32::new(0));

    let consumer_tid = if with_consumer {
        pin_current_cpu();
        let consumer = spawn_consumer(master.as_raw_fd(), tid_slot.clone());
        while tid_slot.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        let consumer_tid = tid_slot.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(300));
        println!(
            "[{attempt}] consumer tid {consumer_tid} blocked, wchan: {}",
            wchan(consumer_tid)
        );
        std::mem::forget(consumer);
        consumer_tid
    } else {
        println!("[{attempt}] control run: no competing reader");
        0
    };

    spawn_watchdog(
        attempt,
        uapi::gettid(),
        consumer_tid,
        slave.as_raw_fd(),
        CqPtrs {
            head: ring.cq_head as usize,
            tail: ring.cq_tail as usize,
        },
        done.clone(),
        hung.clone(),
    );

    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    let mut buf = [0u8; 1];
    ring.submit_read_with_timeout(master.as_raw_fd(), buf.as_mut_ptr(), 1, &timeout);

    let r = unsafe { c::ioctl(slave.as_raw_fd(), c::TCFLSH, TCIFLUSH) };
    println!("[{attempt}] TCFLSH(TCIFLUSH) on slave = {r}, submitting read + 100ms link timeout");

    let ret = ring.enter(2);
    done.store(true, Ordering::SeqCst);
    let was_hung = hung.load(Ordering::SeqCst);
    println!("[{attempt}] io_uring_enter returned {ret}, was_hung={was_hung}");

    let mut seen = 0;
    let iters = if was_hung { 200 } else { 10 };
    for _ in 0..iters {
        if let Some(cqe) = ring.peek_cqe() {
            let name = match cqe.user_data {
                USER_DATA_READ => "read",
                USER_DATA_TIMEOUT => "link-timeout",
                _ => "?",
            };
            let note = match cqe.res {
                -125 if cqe.user_data == USER_DATA_TIMEOUT => " (ECANCELED: timer never fired)",
                -62 if cqe.user_data == USER_DATA_TIMEOUT => " (ETIME: timer fired!)",
                _ => "",
            };
            println!(
                "[{attempt}] cqe {name}: user_data={:#x} res={}{note}",
                cqe.user_data, cqe.res
            );
            seen += 1;
            if seen >= 2 {
                break;
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
    was_hung
}

fn main() {
    let mut attempts: u32 = 5;
    let mut control = false;
    for arg in std::env::args().skip(1) {
        if arg == "--control" {
            control = true;
        } else if let Ok(n) = arg.parse() {
            attempts = n;
        }
    }
    println!("io_uring poll-fallback hang POC (liburing issue #1623)");
    println!();
    println!("- fallback introduced by kernel commit f7c913438533 (\"io_uring/rw: allow pollable");
    println!("  non-blocking attempts for !FMODE_NOWAIT\")");
    println!(
        "- a blocking pty master reader holds ldata->atomic_read_lock while waiting for input"
    );
    println!("- TIOCPKT enables packet mode; TCFLSH on the slave sets slave->ctrl.pktstatus");
    println!("- n_tty_poll() on the master then reports EPOLLIN and io_file_supports_nowait()");
    println!("  trusts that snapshot, issuing the read with IOCB_NOWAIT");
    println!("- the competing reader consumes the status byte, and n_tty_read() ignores");
    println!("  IOCB_NOWAIT, so it blocks inside io_uring_enter()");
    println!("- the read is linked to a 100ms IORING_OP_LINK_TIMEOUT, but io_uring only arms");
    println!("  the timer after the issue callback returns, which never happens while blocked");
    if unsafe { c::geteuid() } != 0 {
        println!();
        println!(
            "note: /proc/<tid>/stack requires CAP_SYS_ADMIN; run with sudo to get kernel stacks"
        );
    }
    println!();
    if control {
        run_attempt(0, false);
        return;
    }
    for attempt in 1..=attempts {
        if run_attempt(attempt, true) {
            println!("[{attempt}] TRIGGERED: io_uring_enter was blocked inside the tty read");
            thread::sleep(Duration::from_millis(300));
            std::process::exit(0);
        }
    }
    println!("no hang after {attempts} attempts");
    std::process::exit(1);
}
