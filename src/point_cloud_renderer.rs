extern crate kiss3d;

use std::fs::File;
use std::io::Read;

use kiss3d::point_renderer::PointRenderer;

use arrow::{array::{UInt16Array, Int32Array, Int64Array, UInt8Array}, ipc::reader::StreamReader, record_batch::RecordBatch};
use kiss3d::camera::Camera;
use kiss3d::nalgebra::Point3;
use kiss3d::planar_camera::PlanarCamera;
use kiss3d::post_processing::PostProcessingEffect;
use kiss3d::renderer::Renderer;
use kiss3d::window::{State, Window};
use pyo3::prelude::*;

use crate::rendering_helpers::{Context, AppConfig};
use crate::interval_tree::ArrayBackedIntervalTree;

/// A coordinate in image space, i.e. a float in the range [0, 1].
/// Used for the rendering part of the code, since that's the type the renderer
/// requires.
pub type ImageCoor = Point3<f32>;

/// A single tag\event that arrives from the Time Tagger.
#[pyclass]
#[derive(Debug, Copy, Clone)]
pub struct Event {
    pub type_: u8,
    pub missed_event: u16,
    pub channel: i32,
    pub time: i64,
}

impl Event {
    /// Create a new Event with the given values
    pub(crate) fn new(type_: u8, missed_event: u16, channel: i32, time: i64) -> Self {
        Event {
            type_,
            missed_event,
            channel,
            time,
        }
    }
}

/// An iterator wrapper for [`EventStream`]
pub(crate) struct EventStreamIter<'a> {
    stream: EventStream<'a>,
    idx: usize,
}

impl<'a> Iterator for EventStreamIter<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        let cur_row = Event::new(
            self.stream.type_.value(self.idx),
            self.stream.missed_events.value(self.idx),
            self.stream.channel.value(self.idx),
            self.stream.time.value(self.idx),
        );
        self.idx += 1;
        Some(cur_row)
    }
}

/// A struct of arrays containing data from the TimeTagger.
///
/// Each field is its own array with some specific data arriving via FFI. Since
/// there are only slices here, the main goal of this stream is to provide easy
/// iteration over the tags for the downstream 'user', via the accompanying 
/// ['EventStreamIter`].
#[derive(Debug)]
pub(crate) struct EventStream<'a> {
    type_: &'a UInt8Array,
    missed_events: &'a UInt16Array,
    channel: &'a Int32Array,
    time: &'a Int64Array,
}

impl<'a> EventStream<'a> {
    /// Creates a new stream with views over the arriving data.
    pub(crate) fn new(
        type_: &'a UInt8Array,
        missed_events: &'a UInt16Array,
        channel: &'a Int32Array,
        time: &'a Int64Array,
    ) -> Self {
        EventStream {
            type_,
            missed_events,
            channel,
            time,
        }
    }

    pub(crate) fn from_streamed_batch(batch: &'a RecordBatch) -> EventStream<'a> {
        let type_ = batch.column(0).as_any().downcast_ref::<UInt8Array>().expect("Type field conversion failed");
        let missed_events = batch.column(1).as_any().downcast_ref::<UInt16Array>().expect("Missed events field conversion failed");
        let channel = batch.column(2).as_any().downcast_ref::<Int32Array>().expect("Channel field conversion failed");
        let time = batch.column(3).as_any().downcast_ref::<Int64Array>().expect("Time field conversion failed");
        EventStream::new(type_, missed_events, channel, time)
    }

    pub(crate) fn iter(self) -> EventStreamIter<'a> {
        EventStreamIter {
            stream: self,
            idx: 0usize,
        }
    }
}

impl<'a> IntoIterator for EventStream<'a> {
    type Item = Event;
    type IntoIter = EventStreamIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Holds the custom renderer that will be used for rendering the
/// point cloud and the needed data streams for it
pub struct AppState<R: Read> {
    point_cloud_renderer: PointRenderer,
    gil: GILGuard,
    data_stream_fh: String,
    tt_module: PyObject,
    context: Context,
    pub data_stream: Option<StreamReader<R>>,
    appconfig: AppConfig,
}

impl AppState<File> {
    /// Generates a new app from a renderer and a receiving end of a channel
    pub fn new(point_cloud_renderer: PointRenderer, tt_module: PyObject, gil: GILGuard, data_stream_fh: String, context: Context) -> Self {
        AppState {
            point_cloud_renderer,
            tt_module,
            gil,
            data_stream_fh,
            context,
            data_stream: None,
            appconfig: AppConfig::new(),
        }
    }

    pub fn start_timetagger_acq(&self) {
        self.tt_module.call0(self.gil.python()).unwrap();
    }

    pub fn acquire_stream_filehandle(&mut self) {
        let stream = File::open(&self.data_stream_fh).unwrap();
        let stream = StreamReader::try_new(stream).expect("Stream file missing");
        self.data_stream = Some(stream);
    }

    /// Convert a raw event tag to a coordinate which will be displayed on the
    /// screen.
    ///
    /// This is the core of the rendering logic of this application, where all
    /// metadata (row, column info) is used to decide where to place a given
    /// event.
    ///
    /// None is returned if the tag isn't a time tag. Currently we discard all
    /// such events.
    pub fn event_to_coordinate(&mut self, event: Event) -> Option<ImageCoor> {
        if event.type_ != 0 {
            return None
        } else {
            todo!()
        }

    }
}

impl State for AppState<File> {
    /// Return the renderer that will be called at each render loop. Without
    /// returning it the loop still runs but the screen is blank.
    fn cameras_and_effect_and_renderer(
        &mut self,
    ) -> (
        Option<&mut dyn Camera>,
        Option<&mut dyn PlanarCamera>,
        Option<&mut dyn Renderer>,
        Option<&mut dyn PostProcessingEffect>,
    ) {
        (None, None, Some(&mut self.point_cloud_renderer), None)
    }

    /// Main logic per step - required by the State trait. The function reads
    /// data awaiting from the TimeTagger and then pushes it into the renderer.
    /// Each recorded tag (=Event) can be a Time tag or a tag signaling
    /// overflow. This iteration process filters these non-time tags from the
    /// more relevant tags.
    fn step(&mut self, _window: &mut Window) {
        if let Some(batch) = self.data_stream.as_mut().unwrap().next() {
            let batch = batch.unwrap();
            let event_stream = EventStream::from_streamed_batch(&batch);
            for event in event_stream.into_iter() {
                if let Some(point) = self.event_to_coordinate(event) {
                    self.point_cloud_renderer.draw_point(point, self.appconfig.point_color)
                }
            }
        }
    }
}

pub fn setup_renderer(gil: GILGuard, tt_module: PyObject, data_stream_fh: String) -> (Window, AppState<File>) {
    let window = Window::new("RPySight");
    let context = Context::new();
    let app = AppState::new(PointRenderer::new(), tt_module, gil, data_stream_fh, context);
    (window, app)
}

#[cfg(test)]
mod tests {
    extern crate numpy;
    extern crate pyo3;
    use std::fs::read_to_string;

    use kiss3d::nalgebra::{Dynamic, U1, MatrixSlice, Scalar};
    use nalgebra_numpy::matrix_slice_from_numpy;
    use numpy::Element;
    use pyo3::{prelude::*, types::PyModule};

    fn get_arr_from_python_file<'a, T: Scalar + Element>(
        arr_name: String, gil: &'a mut GILGuard,
    ) -> MatrixSlice<'a, T, Dynamic, U1, Dynamic, Dynamic> {
        let py = gil.python();
        let python_code = read_to_string("tests/numpy_test.py").expect("No numpy array file found");
        let testfile = PyModule::from_code(py, &python_code, "testing.py", "testarr")
            .expect("Couldn't parse file");
        let arr = testfile.getattr(&arr_name).expect("Array name not found");
        let b = unsafe { matrix_slice_from_numpy::<T, Dynamic, U1>(gil.python(), arr).unwrap() };
        b
    }

}
