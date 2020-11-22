// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//               2018 François Laignel <fengalin@free.fr>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_info, gst_trace};
use gst_base::subclass::prelude::*;

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

use url::Url;

use crate::file_location::FileLocation;

const DEFAULT_LOCATION: Option<FileLocation> = None;

#[derive(Debug)]
struct Settings {
    location: Option<FileLocation>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            location: DEFAULT_LOCATION,
        }
    }
}

static PROPERTIES: [subclass::Property; 1] = [subclass::Property("location", |name| {
    glib::ParamSpec::string(
        name,
        "File Location",
        "Location of the file to write",
        None,
        glib::ParamFlags::READWRITE,
    )
})];

enum State {
    Stopped,
    Started { file: File, position: u64 },
}

impl Default for State {
    fn default() -> State {
        State::Stopped
    }
}

pub struct FileSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

use once_cell::sync::Lazy;
static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rsfilesink",
        gst::DebugColorFlags::empty(),
        Some("File Sink"),
    )
});

impl FileSink {
    fn set_location(
        &self,
        element: &super::FileSink,
        location: Option<FileLocation>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Changing the `location` property on a started `filesink` is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();
        settings.location = match location {
            Some(location) => {
                match settings.location {
                    Some(ref location_cur) => {
                        gst_info!(
                            CAT,
                            obj: element,
                            "Changing `location` from {:?} to {}",
                            location_cur,
                            location,
                        );
                    }
                    None => {
                        gst_info!(CAT, obj: element, "Setting `location` to {}", location,);
                    }
                }
                Some(location)
            }
            None => {
                gst_info!(CAT, obj: element, "Resetting `location` to None",);
                None
            }
        };

        Ok(())
    }
}

impl ObjectSubclass for FileSink {
    const NAME: &'static str = "RsFileSink";
    type Type = super::FileSink;
    type ParentType = gst_base::BaseSink;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::glib_object_subclass!();

    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
        }
    }

    fn type_init(type_: &mut subclass::InitializingType<Self>) {
        type_.add_interface::<gst::URIHandler>();
    }

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "File Sink",
            "Sink/File",
            "Write stream to a file",
            "François Laignel <fengalin@free.fr>, Luis de Bethencourt <luisbg@osg.samsung.com>",
        );

        let caps = gst::Caps::new_any();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for FileSink {
    fn set_property(&self, obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let res = match value.get::<String>() {
                    Ok(Some(location)) => FileLocation::try_from_path_str(location)
                        .and_then(|file_location| self.set_location(obj, Some(file_location))),
                    Ok(None) => self.set_location(obj, None),
                    Err(_) => unreachable!("type checked upstream"),
                };

                if let Err(err) = res {
                    gst_error!(CAT, obj: obj, "Failed to set property `location`: {}", err);
                }
            }
            _ => unimplemented!(),
        };
    }

    fn get_property(&self, _obj: &Self::Type, id: usize) -> glib::Value {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let settings = self.settings.lock().unwrap();
                let location = settings
                    .location
                    .as_ref()
                    .map(|location| location.to_string());

                location.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for FileSink {}

impl BaseSinkImpl for FileSink {
    fn start(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            unreachable!("FileSink already started");
        }

        let settings = self.settings.lock().unwrap();
        let location = settings.location.as_ref().ok_or_else(|| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["File location is not defined"]
            )
        })?;

        let file = File::create(location).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenWrite,
                [
                    "Could not open file {} for writing: {}",
                    location,
                    err.to_string(),
                ]
            )
        })?;
        gst_debug!(CAT, obj: element, "Opened file {:?}", file);

        *state = State::Started { file, position: 0 };
        gst_info!(CAT, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if let State::Stopped = *state {
            return Err(gst_error_msg!(
                gst::ResourceError::Settings,
                ["FileSink not started"]
            ));
        }

        *state = State::Stopped;
        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    // TODO: implement seek in BYTES format

    fn render(
        &self,
        element: &Self::Type,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let (file, position) = match *state {
            State::Started {
                ref mut file,
                ref mut position,
            } => (file, position),
            State::Stopped => {
                gst_element_error!(element, gst::CoreError::Failed, ["Not started yet"]);
                return Err(gst::FlowError::Error);
            }
        };

        gst_trace!(CAT, obj: element, "Rendering {:?}", buffer);
        let map = buffer.map_readable().map_err(|_| {
            gst_element_error!(element, gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        file.write_all(map.as_ref()).map_err(|err| {
            gst_element_error!(
                element,
                gst::ResourceError::Write,
                ["Failed to write buffer: {}", err]
            );
            gst::FlowError::Error
        })?;

        *position += map.len() as u64;

        Ok(gst::FlowSuccess::Ok)
    }
}

impl URIHandlerImpl for FileSink {
    fn get_uri(&self, _element: &Self::Type) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        // Conversion to Url already checked while building the `FileLocation`
        settings.location.as_ref().map(|location| {
            Url::from_file_path(location)
                .expect("FileSink::get_uri couldn't build `Url` from `location`")
                .into_string()
        })
    }

    fn set_uri(&self, element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        // Special case for "file://" as this is used by some applications to test
        // with `gst_element_make_from_uri` if there's an element that supports the URI protocol

        if uri != "file://" {
            let file_location = FileLocation::try_from_uri_str(uri)?;
            self.set_location(&element, Some(file_location))
        } else {
            Ok(())
        }
    }

    fn get_uri_type() -> gst::URIType {
        gst::URIType::Sink
    }

    fn get_protocols() -> Vec<String> {
        vec!["file".to_string()]
    }
}
