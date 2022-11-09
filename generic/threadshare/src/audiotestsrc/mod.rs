use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct AudioTestSrc(ObjectSubclass<imp::AudioTestSrc>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-audiotestsrc",
        gst::Rank::None,
        AudioTestSrc::static_type(),
    )
}
