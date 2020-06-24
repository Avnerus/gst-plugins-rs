// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::prelude::*;

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};

use lazy_static::lazy_static;

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::{u32, u64};

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSink, PadSinkRef, PadSrc, PadSrcRef, PadSrcWeak, Task};

use super::dataqueue::{DataQueue, DataQueueItem};

const DEFAULT_MAX_SIZE_BUFFERS: u32 = 200;
const DEFAULT_MAX_SIZE_BYTES: u32 = 1024 * 1024;
const DEFAULT_MAX_SIZE_TIME: u64 = gst::SECOND_VAL;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    max_size_buffers: u32,
    max_size_bytes: u32,
    max_size_time: u64,
    context: String,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_size_buffers: DEFAULT_MAX_SIZE_BUFFERS,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_size_time: DEFAULT_MAX_SIZE_TIME,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [subclass::Property; 5] = [
    subclass::Property("max-size-buffers", |name| {
        glib::ParamSpec::uint(
            name,
            "Max Size Buffers",
            "Maximum number of buffers to queue (0=unlimited)",
            0,
            u32::MAX,
            DEFAULT_MAX_SIZE_BUFFERS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-size-bytes", |name| {
        glib::ParamSpec::uint(
            name,
            "Max Size Bytes",
            "Maximum number of bytes to queue (0=unlimited)",
            0,
            u32::MAX,
            DEFAULT_MAX_SIZE_BYTES,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-size-time", |name| {
        glib::ParamSpec::uint64(
            name,
            "Max Size Time",
            "Maximum number of nanoseconds to queue (0=unlimited)",
            0,
            u64::MAX - 1,
            DEFAULT_MAX_SIZE_TIME,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context-wait", |name| {
        glib::ParamSpec::uint(
            name,
            "Context Wait",
            "Throttle poll loop to run at most once every this many ms",
            0,
            1000,
            DEFAULT_CONTEXT_WAIT,
            glib::ParamFlags::READWRITE,
        )
    }),
];

#[derive(Debug)]
struct PendingQueue {
    more_queue_space_sender: Option<oneshot::Sender<()>>,
    scheduled: bool,
    items: VecDeque<DataQueueItem>,
}

impl PendingQueue {
    fn notify_more_queue_space(&mut self) {
        self.more_queue_space_sender.take();
    }
}

#[derive(Clone)]
struct QueuePadSinkHandler;

impl PadSinkHandler for QueuePadSinkHandler {
    type ElementImpl = Queue;

    fn sink_chain(
        &self,
        pad: &PadSinkRef,
        _queue: &Queue,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", buffer);
            let queue = Queue::from_instance(&element);
            queue
                .enqueue_item(&element, DataQueueItem::Buffer(buffer))
                .await
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        pad: &PadSinkRef,
        _queue: &Queue,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", list);
            let queue = Queue::from_instance(&element);
            queue
                .enqueue_item(&element, DataQueueItem::BufferList(list))
                .await
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        queue: &Queue,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_debug!(CAT, obj: pad.gst_pad(), "Handling non-serialized {:?}", event);

        if let EventView::FlushStart(..) = event.view() {
            if let Err(err) = queue.task.flush_start() {
                gst_error!(CAT, obj: pad.gst_pad(), "FlushStart failed {:?}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["FlushStart failed {:?}", err]
                );
                return false;
            }
        }

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding non-serialized {:?}", event);
        queue.src_pad.gst_pad().push_event(event)
    }

    fn sink_event_serialized(
        &self,
        pad: &PadSinkRef,
        _queue: &Queue,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling serialized {:?}", event);

        let pad_weak = pad.downgrade();
        let element = element.clone();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            let queue = Queue::from_instance(&element);

            if let EventView::FlushStop(..) = event.view() {
                if let Err(err) = queue.task.flush_stop() {
                    gst_error!(CAT, obj: pad.gst_pad(), "FlushStop failed {:?}", err);
                    gst_element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["FlushStop failed {:?}", err]
                    );
                    return false;
                }
            }

            gst_log!(CAT, obj: pad.gst_pad(), "Queuing serialized {:?}", event);
            queue
                .enqueue_item(&element, DataQueueItem::Event(event))
                .await
                .is_ok()
        }
        .boxed()
    }

    fn sink_query(
        &self,
        pad: &PadSinkRef,
        queue: &Queue,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);

        if query.is_serialized() {
            // FIXME: How can we do this?
            gst_log!(CAT, obj: pad.gst_pad(), "Dropping serialized {:?}", query);
            false
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", query);
            queue.src_pad.gst_pad().peer_query(query)
        }
    }
}

#[derive(Clone, Debug)]
struct QueuePadSrcHandler;

impl QueuePadSrcHandler {
    async fn push_item(
        pad: &PadSrcRef<'_>,
        queue: &Queue,
        item: DataQueueItem,
    ) -> Result<(), gst::FlowError> {
        if let Some(pending_queue) = queue.pending_queue.lock().unwrap().as_mut() {
            pending_queue.notify_more_queue_space();
        }

        match item {
            DataQueueItem::Buffer(buffer) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", buffer);
                pad.push(buffer).await.map(drop)
            }
            DataQueueItem::BufferList(list) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", list);
                pad.push_list(list).await.map(drop)
            }
            DataQueueItem::Event(event) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
                pad.push_event(event).await;
                Ok(())
            }
        }
    }
}

impl PadSrcHandler for QueuePadSrcHandler {
    type ElementImpl = Queue;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        queue: &Queue,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        match event.view() {
            EventView::FlushStart(..) => {
                if let Err(err) = queue.task.flush_start() {
                    gst_error!(CAT, obj: pad.gst_pad(), "FlushStart failed {:?}", err);
                }
            }
            EventView::FlushStop(..) => {
                if let Err(err) = queue.task.flush_stop() {
                    gst_error!(CAT, obj: pad.gst_pad(), "FlushStop failed {:?}", err);
                    gst_element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["FlushStop failed {:?}", err]
                    );
                    return false;
                }
            }
            _ => (),
        }

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
        queue.sink_pad.gst_pad().push_event(event)
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        queue: &Queue,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);

        if let QueryView::Scheduling(ref mut q) = query.view_mut() {
            let mut new_query = gst::query::Scheduling::new();
            let res = queue.sink_pad.gst_pad().peer_query(&mut new_query);
            if !res {
                return res;
            }

            gst_log!(CAT, obj: pad.gst_pad(), "Upstream returned {:?}", new_query);

            let (flags, min, max, align) = new_query.get_result();
            q.set(flags, min, max, align);
            q.add_scheduling_modes(
                &new_query
                    .get_scheduling_modes()
                    .iter()
                    .cloned()
                    .filter(|m| m != &gst::PadMode::Pull)
                    .collect::<Vec<_>>(),
            );
            gst_log!(CAT, obj: pad.gst_pad(), "Returning {:?}", q.get_mut_query());
            return true;
        }

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", query);
        queue.sink_pad.gst_pad().peer_query(query)
    }
}

#[derive(Debug)]
struct QueueTask {
    element: gst::Element,
    src_pad: PadSrcWeak,
    dataqueue: DataQueue,
}

impl QueueTask {
    fn new(element: &gst::Element, src_pad: &PadSrc, dataqueue: DataQueue) -> Self {
        QueueTask {
            element: element.clone(),
            src_pad: src_pad.downgrade(),
            dataqueue,
        }
    }
}

impl TaskImpl for QueueTask {
    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Starting task");

            let queue = Queue::from_instance(&self.element);
            let mut last_res = queue.last_res.lock().unwrap();

            self.dataqueue.start();

            *last_res = Ok(gst::FlowSuccess::Ok);

            gst_log!(CAT, obj: &self.element, "Task started");
            Ok(())
        }
        .boxed()
    }

    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let item = self.dataqueue.next().await;

            let item = match item {
                Some(item) => item,
                None => {
                    gst_log!(CAT, obj: &self.element, "DataQueue Stopped");
                    return Err(gst::FlowError::Flushing);
                }
            };

            let pad = self.src_pad.upgrade().expect("PadSrc no longer exists");
            let queue = Queue::from_instance(&self.element);
            let res = QueuePadSrcHandler::push_item(&pad, &queue, item).await;
            match res {
                Ok(()) => {
                    gst_log!(CAT, obj: &self.element, "Successfully pushed item");
                    *queue.last_res.lock().unwrap() = Ok(gst::FlowSuccess::Ok);
                }
                Err(gst::FlowError::Flushing) => {
                    gst_debug!(CAT, obj: &self.element, "Flushing");
                    *queue.last_res.lock().unwrap() = Err(gst::FlowError::Flushing);
                }
                Err(gst::FlowError::Eos) => {
                    gst_debug!(CAT, obj: &self.element, "EOS");
                    *queue.last_res.lock().unwrap() = Err(gst::FlowError::Eos);
                    pad.push_event(gst::event::Eos::new()).await;
                }
                Err(err) => {
                    gst_error!(CAT, obj: &self.element, "Got error {}", err);
                    gst_element_error!(
                        &self.element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason {}", err]
                    );
                    *queue.last_res.lock().unwrap() = Err(err);
                }
            }

            res
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task");

            let queue = Queue::from_instance(&self.element);
            let mut last_res = queue.last_res.lock().unwrap();

            self.dataqueue.stop();
            self.dataqueue.clear();

            if let Some(mut pending_queue) = queue.pending_queue.lock().unwrap().take() {
                pending_queue.notify_more_queue_space();
            }

            *last_res = Err(gst::FlowError::Flushing);

            gst_log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Starting task flush");

            let queue = Queue::from_instance(&self.element);
            let mut last_res = queue.last_res.lock().unwrap();

            self.dataqueue.clear();

            if let Some(mut pending_queue) = queue.pending_queue.lock().unwrap().take() {
                pending_queue.notify_more_queue_space();
            }

            *last_res = Err(gst::FlowError::Flushing);

            gst_log!(CAT, obj: &self.element, "Task flush started");
            Ok(())
        }
        .boxed()
    }
}

#[derive(Debug)]
struct Queue {
    sink_pad: PadSink,
    src_pad: PadSrc,
    task: Task,
    dataqueue: StdMutex<Option<DataQueue>>,
    pending_queue: StdMutex<Option<PendingQueue>>,
    last_res: StdMutex<Result<gst::FlowSuccess, gst::FlowError>>,
    settings: StdMutex<Settings>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-queue",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing queue"),
    );
}

impl Queue {
    /* Try transfering all the items from the pending queue to the DataQueue, then
     * the current item. Errors out if the DataQueue was full, or the pending queue
     * is already scheduled, in which case the current item should be added to the
     * pending queue */
    fn queue_until_full(
        &self,
        dataqueue: &DataQueue,
        pending_queue: &mut Option<PendingQueue>,
        item: DataQueueItem,
    ) -> Result<(), DataQueueItem> {
        match pending_queue {
            None => dataqueue.push(item),
            Some(PendingQueue {
                scheduled: false,
                ref mut items,
                ..
            }) => {
                let mut failed_item = None;
                while let Some(item) = items.pop_front() {
                    if let Err(item) = dataqueue.push(item) {
                        failed_item = Some(item);
                    }
                }

                if let Some(failed_item) = failed_item {
                    items.push_front(failed_item);

                    Err(item)
                } else {
                    dataqueue.push(item)
                }
            }
            _ => Err(item),
        }
    }

    /* Schedules emptying of the pending queue. If there is an upstream
     * TaskContext, the new task is spawned, it is otherwise
     * returned, for the caller to block on */
    async fn schedule_pending_queue(&self, element: &gst::Element) {
        loop {
            let more_queue_space_receiver = {
                let dataqueue = self.dataqueue.lock().unwrap();
                if dataqueue.is_none() {
                    return;
                }
                let mut pending_queue_grd = self.pending_queue.lock().unwrap();

                gst_log!(CAT, obj: element, "Trying to empty pending queue");

                if let Some(pending_queue) = pending_queue_grd.as_mut() {
                    let mut failed_item = None;
                    while let Some(item) = pending_queue.items.pop_front() {
                        if let Err(item) = dataqueue.as_ref().unwrap().push(item) {
                            failed_item = Some(item);
                        }
                    }

                    if let Some(failed_item) = failed_item {
                        pending_queue.items.push_front(failed_item);
                        let (sender, receiver) = oneshot::channel();
                        pending_queue.more_queue_space_sender = Some(sender);

                        receiver
                    } else {
                        gst_log!(CAT, obj: element, "Pending queue is empty now");
                        *pending_queue_grd = None;
                        return;
                    }
                } else {
                    gst_log!(CAT, obj: element, "Flushing, dropping pending queue");
                    return;
                }
            };

            gst_log!(CAT, obj: element, "Waiting for more queue space");
            let _ = more_queue_space_receiver.await;
        }
    }

    async fn enqueue_item(
        &self,
        element: &gst::Element,
        item: DataQueueItem,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let wait_fut = {
            let dataqueue = self.dataqueue.lock().unwrap();
            let dataqueue = dataqueue.as_ref().ok_or_else(|| {
                gst_error!(CAT, obj: element, "No DataQueue");
                gst::FlowError::Error
            })?;

            let mut pending_queue = self.pending_queue.lock().unwrap();

            if let Err(item) = self.queue_until_full(&dataqueue, &mut pending_queue, item) {
                if pending_queue
                    .as_ref()
                    .map(|pq| !pq.scheduled)
                    .unwrap_or(true)
                {
                    if pending_queue.is_none() {
                        *pending_queue = Some(PendingQueue {
                            more_queue_space_sender: None,
                            scheduled: false,
                            items: VecDeque::new(),
                        });
                    }

                    let schedule_now = match item {
                        DataQueueItem::Event(ref ev) if ev.get_type() != gst::EventType::Eos => {
                            false
                        }
                        _ => true,
                    };

                    pending_queue.as_mut().unwrap().items.push_back(item);

                    gst_log!(
                        CAT,
                        obj: element,
                        "Queue is full - Pushing first item on pending queue"
                    );

                    if schedule_now {
                        gst_log!(CAT, obj: element, "Scheduling pending queue now");
                        pending_queue.as_mut().unwrap().scheduled = true;

                        let wait_fut = self.schedule_pending_queue(element);
                        Some(wait_fut)
                    } else {
                        gst_log!(CAT, obj: element, "Scheduling pending queue later");
                        None
                    }
                } else {
                    pending_queue.as_mut().unwrap().items.push_back(item);
                    None
                }
            } else {
                None
            }
        };

        if let Some(wait_fut) = wait_fut {
            gst_log!(CAT, obj: element, "Blocking until queue has space again");
            wait_fut.await;
        }

        *self.last_res.lock().unwrap()
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let dataqueue = DataQueue::new(
            &element.clone().upcast(),
            self.src_pad.gst_pad(),
            if settings.max_size_buffers == 0 {
                None
            } else {
                Some(settings.max_size_buffers)
            },
            if settings.max_size_bytes == 0 {
                None
            } else {
                Some(settings.max_size_bytes)
            },
            if settings.max_size_time == 0 {
                None
            } else {
                Some(settings.max_size_time)
            },
        );

        *self.dataqueue.lock().unwrap() = Some(dataqueue.clone());

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        self.task
            .prepare(QueueTask::new(element, &self.src_pad, dataqueue), context)
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) {
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.task.unprepare().unwrap();

        *self.dataqueue.lock().unwrap() = None;
        *self.pending_queue.lock().unwrap() = None;

        *self.last_res.lock().unwrap() = Ok(gst::FlowSuccess::Ok);

        gst_debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }
}

impl ObjectSubclass for Queue {
    const NAME: &'static str = "RsTsQueue";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing queue",
            "Generic",
            "Simple data queue",
            "Sebastian Dröge <sebastian@centricular.com>",
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

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.get_pad_template("sink").unwrap(), Some("sink")),
                QueuePadSinkHandler,
            ),
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.get_pad_template("src").unwrap(), Some("src")),
                QueuePadSrcHandler,
            ),
            task: Task::default(),
            dataqueue: StdMutex::new(None),
            pending_queue: StdMutex::new(None),
            last_res: StdMutex::new(Ok(gst::FlowSuccess::Ok)),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for Queue {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        let mut settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("max-size-buffers", ..) => {
                settings.max_size_buffers = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-bytes", ..) => {
                settings.max_size_bytes = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-time", ..) => {
                settings.max_size_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("context", ..) => {
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        let settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("max-size-buffers", ..) => Ok(settings.max_size_buffers.to_value()),
            subclass::Property("max-size-bytes", ..) => Ok(settings.max_size_bytes.to_value()),
            subclass::Property("max-size-time", ..) => Ok(settings.max_size_time.to_value()),
            subclass::Property("context", ..) => Ok(settings.context.to_value()),
            subclass::Property("context-wait", ..) => Ok(settings.context_wait.to_value()),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.sink_pad.gst_pad()).unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();
    }
}

impl ElementImpl for Queue {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToPaused {
            self.start(element).map_err(|_| gst::StateChangeError)?;
        }

        Ok(success)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(Some(plugin), "ts-queue", gst::Rank::None, Queue::get_type())
}
