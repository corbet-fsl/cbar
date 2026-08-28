# Ironbar launch service

This small library gives UI modules a bounded, nonblocking, shell-free way to
hand an exact argument vector to an application. Linux systems with a usable
systemd user manager receive a collected transient service; other systems use
a portable direct child that remains owned by an internal reaper.

`submit_detached_argv` validates and atomically enqueues one request.
`submit_detached_batch` does the same for up to 64 requests: every argv and the
aggregate batch are admitted under one reservation, or none can reach a
worker. Both APIs return input-order tickets that are futures, so GTK callers
can await each handoff without occupying a blocking runtime thread. Queue
saturation, manager rejection, and direct-child saturation are explicit
errors. A capability probe that cannot resolve within its hard deadline safely
selects direct launch because the requested argv has not yet been submitted.

The launch pool uses twice the process's available parallelism, bounded from
two to 32 workers. Its FIFO is independently bounded from 64 to 128 pending
requests (four slots per active worker, clamped to those limits), in addition
to the active handoffs.
Queued argv are also capped at 32,768 entries and 4 MiB. An individual argv is
limited to 4,096 entries and 256 KiB; an atomic batch is limited to 32,768
entries and 4 MiB, enough for 64 launcher entries at its 64 KiB inventory
limit. These independent bounds keep manager acknowledgements
concurrent without turning a stalled manager or a large desktop entry into an
unbounded thread or memory backlog.

The direct backend owns at most 512 simultaneous children. That bound is high
enough for long-lived desktop applications without allowing a click storm to
grow process bookkeeping or file-descriptor use indefinitely. Exceeding it is
reported before another process is spawned. Linux child exits normally wake
the single reaper through pidfds; a two-second scan is only the compatibility
path for older kernels and other operating systems.

A direct child is placed in its own process group and survives an ordinary
parent exit on POSIX, but it necessarily inherits the caller's existing cgroup,
resource limits and sandbox. Only the user-service backend provides a new
service lifetime and policy boundary. Linux packages that promise that boundary
must ship a working user manager and `systemd-run`; the direct backend remains
the bounded portability path for systems without one.

The manager backend copies every representable variable present in the
caller's environment and preserves the caller's working directory. A systemd
user manager can additionally supply its own baseline and service metadata
variables, so the set of variables is not promised to be byte-for-byte
identical to a direct child. Values are never placed in helper argv.
