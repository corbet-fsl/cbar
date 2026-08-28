// cbar-launcher-core — the launcher, with no toolkit in it.
//
// WHY THIS IS A SEPARATE CRATE. Everything that makes cbar's launcher what it is lives here: the matrix
// and how you move through it, what an appset is, where a dragged application ends up, which
// things are used often enough to move and by how much, what a query matches, and what argv
// actually gets executed. None of that has anything to do with GTK, Wayland, or Linux.
//
// It was already true by convention -- model.rs has carried "nothing in this file may import gtk"
// since the beginning, and the 56 tests here run without a display. Making it a crate turns a
// convention into a boundary the compiler enforces, which matters now for two reasons:
//
//   * The shell may not stay GTK. GTK is the largest remaining cost -- a ~47MB floor and ~85ms of
//     initialisation before this program does anything -- and whether a portable toolkit beats it
//     is a question to answer with measurements, not with a rewrite nobody can back out of.
//   * macOS has no layer shell at all. The analogue there is a non-activating panel, so a Mac port
//     needs a different surface whatever the toolkit; what it does NOT need is a second copy of
//     the matrix, the placement rules or the frecency gate.
//
// So: one core, many shells. A shell's job is to draw this and to send keystrokes and drops back
// into it, and nothing here should ever need to know which one is attached.
pub mod config;
pub mod keymap;
pub mod model;
pub mod usage;
