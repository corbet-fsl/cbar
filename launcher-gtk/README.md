# cbar GTK launcher

This crate owns the GTK4 matrix-launcher window embedded in cbar's existing
application process. It consumes the toolkit-independent launcher core,
streams independent provider results, and submits exact argument vectors to
the shared bounded launch service.

It is an optional cbar feature; Ironbar-compatible configurations that do not
enable or configure the launcher retain their existing runtime behavior.
