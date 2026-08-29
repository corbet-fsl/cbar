Ironvars are runtime variables that can be referenced in several places in your config, 
then set using the IPC server (such as via the CLI) using the `set` command.

Keys can consist of alphanumeric characters, `-` and `_` only.
Any UTF-8 string is a valid value.

Reference values using `#my_variable`. These update as soon as the value changes.

You can set defaults using the `ironvar_defaults` key in your top-level config.

Some modules (such as `sys_info`) expose their values over the Ironvar interface,
allowing you to build custom interfaces and integrate into scripts.
These present their values inside read-only namespaces.

Some examples below:

```shell
cbar var list
cbar var list sysinfo
cbar var list sysinfo.disk_percent
cbar var get sysinfo.disk_percent./home
cbar var get sysinfo.disk_percent.mean
cbar var get sysinfo.memory_percent
```
