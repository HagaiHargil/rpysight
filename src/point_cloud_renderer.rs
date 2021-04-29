extern crate kiss3d;

use std::fs::File;
use std::io::Read;

use anyhow::{Context, Result};
use arrow::{ipc::reader::StreamReader, record_batch::RecordBatch};
use kiss3d::camera::Camera;
use kiss3d::planar_camera::PlanarCamera;
use kiss3d::point_renderer::PointRenderer;
use kiss3d::post_processing::PostProcessingEffect;
use kiss3d::renderer::Renderer;
use kiss3d::window::{State, Window};
use kiss3d::text::Font;
use nalgebra::{Point3, Point2};

use crate::configuration::{AppConfig, DataType, Inputs};
use crate::rendering_helpers::{Picosecond, TimeToCoord};
use crate::GLOBAL_OFFSET;
use crate::event_stream::{EventStreamIter, Event, EventStream};

/// A coordinate in image space, i.e. a float in the range [0, 1].
/// Used for the rendering part of the code, since that's the type the renderer
/// requires.
pub type ImageCoor = Point3<f32>;

/// A handler of streaming time tagger data
pub trait TimeTaggerIpcHandler {
    fn acquire_stream_filehandle(&mut self) -> Result<()>;
    fn event_to_coordinate(&mut self, event: Event) -> ProcessedEvent;
    fn get_event_stream<'a>(&mut self, batch: &'a RecordBatch) -> Option<EventStream<'a>>;
}

/// The result of handling an event generated by the time tagger.
///
/// Each event might arrive from different channels which require different
/// handling, and this enum contains all possible actions we might want to do
/// with these results.
#[derive(Debug, Clone, Copy)]
pub enum ProcessedEvent {
    /// Contains the coordinates in image space and the color
    Displayed(Point3<f32>, Point3<f32>),
    /// Nothing to do with this event
    NoOp,
    /// Start drawing a new frame due to a line signal that belongs to the
    /// next frame (> num_rows)
    LineNewFrame,
    /// Start drawing a new frame due to a photon signal with a time after the
    /// end of the current frame. Probably means that we didn't record all line
    /// signals that arrived during the frame
    PhotonNewFrame,
    /// Erroneuous event, usually for tests
    Error,
    /// First line encoutered and its timing
    FirstLine(Picosecond),
}

/// Implemented by Apps who wish to display points
pub trait PointDisplay {
    fn new() -> Self;
    fn display_point(&mut self, p: Point3<f32>, c: Point3<f32>, time: Picosecond);
}

/// Holds the custom renderer that will be used for rendering the
/// point cloud
pub struct DisplayChannel<T: PointDisplay + Renderer> {
    pub window: Window,
    pub renderer: T,
}

impl<T: PointDisplay + Renderer> DisplayChannel<T> {
    pub fn new(channel_name: &str, frame_rate: u64) -> Self {
        let mut window = Window::new(channel_name);
        window.set_framerate_limit(Some(frame_rate));
        DisplayChannel { window, renderer: T::new() }
    }

    #[inline]
    pub fn display_point(&mut self, p: Point3<f32>, c: Point3<f32>, _time: Picosecond) {
        self.window.draw_point(&p, &c)
    }

    pub fn render(&mut self) {
        self.window.render();
    }

    pub fn get_window(&mut self) -> &mut Window {
        &mut self.window
    }
}

/// Main struct that holds the renderers and the needed data streams for
/// them
pub struct AppState<T: PointDisplay + Renderer, R: Read> {
    pub channel1: DisplayChannel<T>,
    pub channel2: DisplayChannel<T>,
    pub channel3: DisplayChannel<T>,
    pub channel4: DisplayChannel<T>,
    pub channel_merge: DisplayChannel<T>,
    data_stream_fh: String,
    pub data_stream: Option<StreamReader<R>>,
    time_to_coord: TimeToCoord,
    inputs: Inputs,
    appconfig: AppConfig,
    rows_per_frame: u32,
    row_count: u32,
    last_line: Picosecond,
    lines_vec: Vec<Picosecond>,
}

impl<T: PointDisplay + Renderer> AppState<T, File> {
    /// Generates a new app from a renderer and a receiving end of a channel
    pub fn new(
        channel_names: Option<&[&str]>,
        data_stream_fh: String,
        appconfig: AppConfig,
    ) -> Self {
        let frame_rate = appconfig.frame_rate().round() as u64;
        let channel_names = channel_names.unwrap_or(&["Channel 1", "Channel 2", "Channel 3", "Channel 4", "Channel Merge"]);
        AppState {
            channel1: DisplayChannel::new(channel_names[0], frame_rate),
            channel2: DisplayChannel::new(channel_names[1], frame_rate),
            channel3: DisplayChannel::new(channel_names[2], frame_rate),
            channel4: DisplayChannel::new(channel_names[3], frame_rate),
            channel_merge: DisplayChannel::new(channel_names[4], frame_rate),
            data_stream_fh,
            data_stream: None,
            time_to_coord: TimeToCoord::from_acq_params(&appconfig, GLOBAL_OFFSET),
            inputs: Inputs::from_config(&appconfig),
            appconfig: appconfig.clone(),
            rows_per_frame: appconfig.rows,
            row_count: 0,
            last_line: 0,
            lines_vec: Vec::<Picosecond>::with_capacity(3000),
        }
    }

    /// Called when an event from the line channel arrives to the event stream.
    ///
    /// It handles the first line of the experiment, by returning a special
    /// signal, a standard line in the middle of the frame or a line which
    /// is the first in the next frame's line count.
    fn handle_line_event(&mut self, event: Event) -> ProcessedEvent {
        if self.last_line == 0 {
            self.row_count = 1;
            self.lines_vec.push(event.time);
            self.last_line = event.time;
            info!("Found the first line of the stream: {:?}", event);
            return ProcessedEvent::FirstLine(event.time);
        }
        let time = event.time;
        debug!("Elapsed time since last line: {}", time - self.last_line);
        self.last_line = time;
        if self.row_count == self.rows_per_frame {
            self.row_count = 0;
            debug!("Here are the lines: {:#?}", self.lines_vec);
            self.lines_vec.clear();
            ProcessedEvent::LineNewFrame
        } else {
            self.row_count += 1;
            self.lines_vec.push(time);
            ProcessedEvent::NoOp
        }
    }

    pub fn populate_single_frame(&mut self, mut events_after_newframe: Option<Vec<Event>>) -> Option<Vec<Event>> {
        if let Some(ref previous_events) = events_after_newframe {
            debug!("Looking for leftover events");
            // Start with the leftover events from the previous frame
            for event in previous_events.iter().by_ref() {
                match self.event_to_coordinate(*event) {
                    ProcessedEvent::Displayed(p, c) => self.channel_merge.display_point(p, c, event.time),
                    ProcessedEvent::NoOp => continue,
                    ProcessedEvent::LineNewFrame => {
                        info!("New frame due to line");
                        let new_events_after_newframe = Some(previous_events.iter().copied().collect::<Vec<Event>>());
                        self.time_to_coord.update_2d_data_for_next_frame();
                        return new_events_after_newframe
                    },
                    ProcessedEvent::PhotonNewFrame => {
                        let new_events_after_newframe = Some(previous_events.iter().copied().collect::<Vec<Event>>());
                        self.time_to_coord.update_2d_data_for_next_frame();
                        self.event_to_coordinate(*event);
                        self.lines_vec.clear();
                        self.row_count = 0;
                        return new_events_after_newframe
                    }
                    ProcessedEvent::FirstLine(time) => {
                        error!("First line already detected! {}", time);
                        continue;
                    }
                    ProcessedEvent::Error => {
                        error!("Received an erroneuous event: {:?}", event);
                        continue;
                    }
                }
            }
        }
        // New experiments will start out here, by loading the data and
        // looking for the first line signal
        'frame: loop {
            debug!("Starting a 'frame loop");
            let batch = match self.data_stream.as_mut().unwrap().next() {
                Some(batch) => batch.expect("Couldn't extract batch from stream"),
                None => return None,
            };
            let event_stream = match self.get_event_stream(&batch) {
                Some(stream) => stream,
                None => return None,
            };
            let mut event_stream = event_stream.into_iter();
            if self.last_line == 0 {
                debug!("First line has not been found yet");
                match event_stream.position(|event| self.find_first_line(&event)) {
                    Some(_) => { },  // .position() advances the iterator for us
                    None => return None,  // we need more data since this batch has no first line
                };
            }
            // match self.check_relevance_of_batch(&event_stream) {
            //     true => {}
            //     false => continue,
            // };
            info!("Starting iteration on this stream");
            for event in event_stream.by_ref() {
                match self.event_to_coordinate(event) {
                    ProcessedEvent::Displayed(p, c) => self.channel_merge.display_point(p, c, event.time),
                    ProcessedEvent::NoOp => continue,
                    ProcessedEvent::PhotonNewFrame => {
                        events_after_newframe = Some(event_stream.collect::<Vec<Event>>());
                        info!("We're in a photonewframe sit!");
                        self.time_to_coord.update_2d_data_for_next_frame();
                        self.event_to_coordinate(event);
                        self.lines_vec.clear();
                        self.row_count = 0;
                        break 'frame;
                    }, 
                    ProcessedEvent::LineNewFrame => {
                        info!("New frame due to line");
                        events_after_newframe = Some(event_stream.collect::<Vec<Event>>());
                        self.time_to_coord.update_2d_data_for_next_frame();
                        break 'frame;
                    }
                    ProcessedEvent::FirstLine(time) => {
                        error!("First line already detected! {}", time);
                        continue;
                    }
                    ProcessedEvent::Error => {
                        error!("Received an erroneuous event: {:?}", event);
                        continue;
                    }
                }
            }
        }
        debug!("Returning the leftover events ({:?}) of them", &events_after_newframe);
        events_after_newframe
    }

    pub fn start_acq_loop_for(&mut self, steps: usize) -> Result<()> {
        self.acquire_stream_filehandle()?;
        let mut events_after_newframe = None;
        for _ in 0..steps {
            debug!("Starting step");
            events_after_newframe = self.populate_single_frame(events_after_newframe);
            debug!("Calling render");
            self.channel_merge.window.draw_text("DFDFDFDFDF", &Point2::<f32>::new(0.5, 0.5), 60.0, &Font::default(), &Point3::new(1.0, 1.0, 1.0));
            self.channel_merge.window.draw_point(&Point3::<f32>::new(0.5, 0.5, 0.5), &Point3::<f32>::new(1.0, 1.0, 1.0));
            self.channel_merge.render();
        };
        info!("Acq loop done");
        Ok(())
    }

    /// Main
    pub fn start_inf_acq_loop(&mut self) -> Result<()> {
        self.acquire_stream_filehandle()?;
        let mut events_after_newframe = None;
        'acquisition: loop {
            events_after_newframe = self.populate_single_frame(events_after_newframe);
            // self.channel1.render();
            // self.channel2.render();
            // self.channel3.render();
            // self.channel4.render();
            self.channel_merge.render();
        };
    }

    /// Verifies that the current event stream lies within the boundaries of
    /// the current frame we're trying to render.
    fn check_relevance_of_batch(&self, event_stream: &EventStream) -> bool {
        if let Some(event) = Event::from_stream_idx(&event_stream, event_stream.num_rows() - 1) {
            if event.time <= self.time_to_coord.earliest_frame_time {
                debug!("The last event in the batch arrived before the first in the frame: received event: {}, earliest in frame: {}", event.time ,self.time_to_coord.earliest_frame_time);
                false
            } else {
                true
            }
        } else {
            error!("For some reason no last event exists in this stream");
            false
        }
    }

    fn find_first_line(&mut self, event: &Event) -> bool {
        match self.event_to_coordinate(*event) {
            ProcessedEvent::FirstLine(time) => {
                self.time_to_coord = TimeToCoord::from_acq_params(&self.appconfig, time);
                true
            }
            _ => false,
        }
    }
}

impl PointDisplay for PointRenderer {
    fn new() -> Self {
        PointRenderer::new()
    }

    #[inline]
    fn display_point(&mut self, p: Point3<f32>, c: Point3<f32>, _time: Picosecond) {
        self.draw_point(p, c);
    }
}

impl<T: PointDisplay + Renderer> TimeTaggerIpcHandler for AppState<T, File> {
    /// Instantiate an IPC StreamReader using an existing file handle.
    fn acquire_stream_filehandle(&mut self) -> Result<()> {
        let stream =
            File::open(&self.data_stream_fh).context("Can't open stream file, exiting.")?;
        let stream =
            StreamReader::try_new(stream).context("Stream file missing, cannot recover.")?;
        self.data_stream = Some(stream);
        debug!("File handle for stream acquired!");
        Ok(())
    }

    /// Convert a raw event tag to a coordinate which will be displayed on the
    /// screen.
    ///
    /// This is the core of the rendering logic of this application, where all
    /// metadata (row, column info) is used to decide where to place a given
    /// event.
    ///
    /// None is returned if the tag isn't a time tag. When the tag is from a
    /// non-imaging channel it's taken into account, but otherwise (i.e. in
    /// cases of overflow it's discarded at the moment.
    fn event_to_coordinate(&mut self, event: Event) -> ProcessedEvent {
        if event.type_ != 0 {
            warn!("Event type was not a time tag: {:?}", event);
            return ProcessedEvent::NoOp;
        }
        debug!("Received the following event: {:?}", event);
        match self.inputs[event.channel] {
            DataType::Pmt1 => self.time_to_coord.tag_to_coord_linear(event.time, 0),
            DataType::Pmt2 => self.time_to_coord.tag_to_coord_linear(event.time, 1),
            DataType::Pmt3 => self.time_to_coord.tag_to_coord_linear(event.time, 2),
            DataType::Pmt4 => self.time_to_coord.tag_to_coord_linear(event.time, 3),
            DataType::Line => self.handle_line_event(event),
            DataType::TagLens => self.time_to_coord.new_taglens_period(event.time),
            DataType::Laser => self.time_to_coord.new_laser_event(event.time),
            DataType::Frame => ProcessedEvent::NoOp,
            DataType::Invalid => {
                warn!("Unsupported event: {:?}", event);
                ProcessedEvent::NoOp
            }
        }
    }

    #[inline]
    fn get_event_stream<'b>(&mut self, batch: &'b RecordBatch) -> Option<EventStream<'b>> {
        info!("Received {} rows", batch.num_rows());
        let event_stream = EventStream::from_streamed_batch(batch);
        if event_stream.num_rows() == 0 {
            debug!("A batch with 0 rows was received");
            None
        } else {
            Some(event_stream)
        }
    }
}
