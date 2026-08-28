# nixlaunch origin

The toolkit-independent core in this directory and the Golden Master GTK shell in
`../launcher-gtk` were incorporated from
[`julian-corbet/nixlaunch-corbet-ch`](https://github.com/julian-corbet/nixlaunch-corbet-ch) at
commit `8168771811a225448d682113379f91ef1373e7ae`.

It remains a separate local crate so matrix, navigation, search, placement, visibility, keymap,
frecency, configuration, and argv behavior have one implementation and their original headless
test suite remains runnable. It is linked into the cbar process; it is not a daemon or subprocess.
The original MIT notice is preserved in `LICENSE` beside this file.
