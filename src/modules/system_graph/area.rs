//! GTK sizing contract for the responsive graph canvas.
//!
//! `GtkDrawingArea::content-width` is a hard content request, so it cannot
//! represent a small minimum and a larger natural width at the same time. A
//! tiny subclass supplies that missing minimum/natural measure pair while the
//! inherited draw callback still receives the real allocation.

use glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{Accessible, Buildable, ConstraintTarget, DrawingArea, Orientation, Widget};
use std::cell::Cell;

mod imp {
    use super::*;

    #[derive(Debug)]
    pub struct ResponsiveGraphArea {
        pub(super) minimum_width: Cell<i32>,
        pub(super) natural_width: Cell<i32>,
        pub(super) height: Cell<i32>,
    }

    impl Default for ResponsiveGraphArea {
        fn default() -> Self {
            Self {
                minimum_width: Cell::new(1),
                natural_width: Cell::new(1),
                height: Cell::new(1),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResponsiveGraphArea {
        const NAME: &'static str = "CbarSystemGraphArea";
        type Type = super::ResponsiveGraphArea;
        type ParentType = DrawingArea;
    }

    impl ObjectImpl for ResponsiveGraphArea {}

    impl WidgetImpl for ResponsiveGraphArea {
        fn measure(&self, orientation: Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == Orientation::Horizontal {
                let minimum = self.minimum_width.get().max(1);
                let natural = self.natural_width.get().max(minimum);
                (minimum, natural, -1, -1)
            } else {
                let height = self.height.get().max(1);
                (height, height, -1, -1)
            }
        }
    }

    impl DrawingAreaImpl for ResponsiveGraphArea {}
}

glib::wrapper! {
    pub struct ResponsiveGraphArea(ObjectSubclass<imp::ResponsiveGraphArea>)
        @extends DrawingArea, Widget,
        @implements Accessible, Buildable, ConstraintTarget;
}

impl ResponsiveGraphArea {
    pub fn new(minimum_width: i32, natural_width: i32, height: i32) -> Self {
        let area: Self = glib::Object::new();
        area.set_measure(minimum_width, natural_width, height);
        area
    }

    pub fn set_widths(&self, minimum_width: i32, natural_width: i32) {
        let imp = self.imp();
        let minimum_width = minimum_width.max(1);
        let natural_width = natural_width.max(minimum_width);
        let minimum_changed = imp.minimum_width.replace(minimum_width) != minimum_width;
        let natural_changed = imp.natural_width.replace(natural_width) != natural_width;
        if minimum_changed || natural_changed {
            self.queue_resize();
        }
    }

    fn set_measure(&self, minimum_width: i32, natural_width: i32, height: i32) {
        let imp = self.imp();
        imp.minimum_width.set(minimum_width.max(1));
        imp.natural_width
            .set(natural_width.max(imp.minimum_width.get()));
        imp.height.set(height.max(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_measure_exposes_a_real_shrink_range() {
        let imp = imp::ResponsiveGraphArea::default();
        imp.minimum_width.set(163);
        imp.natural_width.set(1196);
        imp.height.set(26);

        assert_eq!(
            imp.measure(Orientation::Horizontal, -1),
            (163, 1196, -1, -1)
        );
        assert_eq!(imp.measure(Orientation::Vertical, 850), (26, 26, -1, -1));
    }
}
