// Remember to  $Env:PYTHONHOME = "C:\Users\PBLab\.conda\envs\timetagger\"
// because powershell is too dumb to remember.
use std::path::PathBuf;

use pyo3::prelude::*;

use librpysight::point_cloud_renderer::setup_renderer;
use librpysight::load_timetagger_module;
use librpysight::gui::run_appconfig_gui;

const TT_DATA_STREAM: &'static str = "__tt_data_stream.dat";

fn main() -> Result<(), std::io::Error> {
    // Set up the Python side
    let filename = PathBuf::from("rpysight/call_timetagger.py");
    let r = dbg!(run_appconfig_gui().unwrap());
    let timetagger_module: PyObject = load_timetagger_module(filename)?;
    let gil = Python::acquire_gil();
    // Set up the renderer side
    let (window, mut app) = setup_renderer(gil, timetagger_module, TT_DATA_STREAM.into(),);
    app.start_timetagger_acq();
    app.acquire_stream_filehandle(); 
    window.render_loop(app);
    Ok(())
}
