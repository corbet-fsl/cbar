Cbar includes a local IPC server for runtime control. The command-line client
is part of the same `cbar` binary and is generated from the IPC definition.

# CLI

Use `cbar --help` for the full command tree and per-command help such as
`cbar launcher --help` or `cbar var set --help`.

The CLI supports plain-text and JSON output. Plain text prints `ok` for an empty
success, one line per returned value, and `error` followed by the message on
standard error for failures.

```shell
$ cbar var set subject world
ok

$ cbar var get subject
world

$ cbar launcher show
ok
```

All error responses exit with status 3. The control socket is
`$XDG_RUNTIME_DIR/cbar-ipc.sock` and is accepted only when the runtime directory
is private and owned by the effective user.
