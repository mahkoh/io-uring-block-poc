io_uring poll-fallback hang POC (liburing issue #1623)

- fallback introduced by kernel commit f7c913438533 ("io_uring/rw: allow pollable
  non-blocking attempts for !FMODE_NOWAIT")
- a blocking pty master reader holds ldata->atomic_read_lock while waiting for input
- TIOCPKT enables packet mode; TCFLSH on the slave sets slave->ctrl.pktstatus
- n_tty_poll() on the master then reports EPOLLIN and io_file_supports_nowait()
  trusts that snapshot, issuing the read with IOCB_NOWAIT
- the competing reader consumes the status byte, and n_tty_read() ignores
  IOCB_NOWAIT, so it blocks inside io_uring_enter()
- the read is linked to a 100ms IORING_OP_LINK_TIMEOUT, but io_uring only arms
  the timer after the issue callback returns, which never happens while blocked

[1] master flags 0x8002 (O_NONBLOCK=false)
[1] consumer tid 2715156 blocked, wchan: wait_woken
[1] TCFLSH(TCIFLUSH) on slave = 0, submitting read + 100ms link timeout
[1] HANG: io_uring_enter has not returned for 3s
[1] 100ms link timeout did not fire: 0 CQEs after 3s (read=0x1623, timeout=0x1624)
[1] submitter tid 2715155 in syscall 426 (426=io_uring_enter), wchan: wait_woken
[1] consumer  tid 2715156 wchan: <unavailable>
[1] submitter kernel stack:
[<0>] wait_woken+0x85/0x90
[<0>] n_tty_read+0x52b/0x6e0
[<0>] tty_read+0x174/0x370
[<0>] __io_read+0xc1/0x490
[<0>] io_read+0x3f/0x110
[<0>] __io_issue_sqe+0x3b/0x1b0
[<0>] io_issue_sqe+0x2f/0x580
[<0>] io_submit_sqes+0x26c/0x780
[<0>] __do_sys_io_uring_enter+0x345/0x840
[<0>] do_syscall_64+0xaa/0x660
[<0>] entry_SYSCALL_64_after_hwframe+0x76/0x7e
[1] consumer kernel stack:
<unavailable: No such file or directory (os error 2)>
[1] fed 1 byte into the slave (write=1)
[1] io_uring_enter returned 2, was_hung=true
[1] cqe read: user_data=0x1623 res=1
[1] cqe link-timeout: user_data=0x1624 res=-125 (ECANCELED: timer never fired)
[1] TRIGGERED: io_uring_enter was blocked inside the tty read
[1] io_uring_enter returned after input was fed
[1] TRIGGERED: io_uring_enter was blocked inside the tty read
