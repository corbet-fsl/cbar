# Contributing to cbar

Contributions are welcome. Please discuss large behavior or architecture
changes in an issue before investing in an implementation.

Code changes must:

- preserve the supported Ironbar configuration and module surface unless a
  deliberate cbar breaking release says otherwise;
- keep compositor-specific behavior behind its typed adapter;
- remain capability-driven and free of host-specific policy;
- avoid blocking work on the GTK main thread;
- include behavior tests for new failure and recovery paths; and
- pass `checks/release-matrix.sh` before release.

Use conventional commit subjects. Keep commits focused and update the relevant
architecture or user documentation when behavior changes.

Bug reports should include the cbar version, compositor, distribution,
configuration format, reproduction steps, and a log captured with `CBAR_LOG`
or `CBAR_FILE_LOG` set to `debug`.
