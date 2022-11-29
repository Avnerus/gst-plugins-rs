# Gtk 4 Sink & Paintable

GTK 4 provides `gtk::Video` & `gtk::Picture` for rendering media such as videos. As the default `gtk::Video` widget doesn't
offer the possibility to use a custom `gst::Pipeline`. The plugin provides a `gst_video::VideoSink` along with a `gdk::Paintable` that's capable of rendering the sink's frames.

The Sink can generate GL Textures if the system is capable of it, but it needs to be compiled
with either `wayland`, `x11glx` or `x11egl` cargo features.
