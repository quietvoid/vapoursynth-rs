extern crate failure;

use failure::{err_msg, Error, ResultExt};

#[cfg(all(feature = "vsscript-functions",
          any(feature = "vapoursynth-functions", feature = "gte-vsscript-api-32")))]
mod inner {
    extern crate clap;
    extern crate vapoursynth;

    use std::cmp;
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::fmt::Debug;
    use std::fs::File;
    use std::io::{self, stdout, Stdout, Write};
    use std::sync::{Arc, Condvar, Mutex};

    use self::clap::{App, Arg};
    use self::vapoursynth::vsscript::{Environment, EvalFlags};
    use self::vapoursynth::{Frame, Node, OwnedMap, Property, API};
    use self::vapoursynth::node::GetFrameError;
    use self::vapoursynth::format::{ColorFamily, SampleType};
    use super::*;

    enum OutputTarget {
        File(File),
        Stdout(Stdout),
        Empty,
    }

    struct OutputParameters {
        node: Node,
        alpha_node: Option<Node>,
        start_frame: usize,
        end_frame: usize,
        requests: usize,
        y4m: bool,
        progress: bool,
    }

    struct OutputState {
        output_target: OutputTarget,
        timecodes_file: Option<File>,
        error: Option<(usize, Error)>,
        reorder_map: HashMap<usize, (Option<Frame>, Option<Frame>)>,
        last_requested_frame: usize,
        next_output_frame: usize,
    }

    struct SharedData {
        output_done_pair: (Mutex<bool>, Condvar),
        output_parameters: OutputParameters,
        output_state: Mutex<OutputState>,
    }

    impl Write for OutputTarget {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match *self {
                OutputTarget::File(ref mut file) => file.write(buf),
                OutputTarget::Stdout(ref mut out) => out.write(buf),
                OutputTarget::Empty => Ok(buf.len()),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            match *self {
                OutputTarget::File(ref mut file) => file.flush(),
                OutputTarget::Stdout(ref mut out) => out.flush(),
                OutputTarget::Empty => Ok(()),
            }
        }
    }

    fn print_version() -> Result<(), Error> {
        let environment = Environment::new().context("Couldn't create the VSScript environment")?;
        let core = environment
            .get_core()
            .context("Couldn't create the VapourSynth core")?;
        print!("{}", core.info().version_string);

        Ok(())
    }

    // Parses the --arg arguments in form of key=value.
    fn parse_arg(arg: &str) -> Result<(&str, &str), Error> {
        arg.find('=')
            .map(|index| arg.split_at(index))
            .map(|(k, v)| (k, &v[1..]))
            .ok_or(err_msg(format!("No value specified for argument: {}", arg)))
    }

    // Returns "Variable" or the value of the property passed through a function.
    fn map_or_variable<T, F>(x: &Property<T>, f: F) -> String
    where
        T: Debug + Clone + Copy + Eq + PartialEq,
        F: FnOnce(&T) -> String,
    {
        match *x {
            Property::Variable => "Variable".to_owned(),
            Property::Constant(ref x) => f(x),
        }
    }

    fn print_info(writer: &mut Write, node: &Node, alpha: Option<&Node>) -> Result<(), Error> {
        let info = node.info();

        writeln!(
            writer,
            "Width: {}",
            map_or_variable(&info.resolution, |x| format!("{}", x.width))
        )?;
        writeln!(
            writer,
            "Height: {}",
            map_or_variable(&info.resolution, |x| format!("{}", x.height))
        )?;

        #[cfg(feature = "gte-vapoursynth-api-32")]
        writeln!(writer, "Frames: {}", info.num_frames)?;

        #[cfg(not(feature = "gte-vapoursynth-api-32"))]
        writeln!(
            writer,
            "Frames: {}",
            match info.num_frames {
                Property::Variable => "Unknown".to_owned(),
                Property::Constant(x) => format!("{}", x),
            }
        )?;

        writeln!(
            writer,
            "FPS: {}",
            map_or_variable(&info.framerate, |x| format!(
                "{}/{} ({:.3} fps)",
                x.numerator,
                x.denominator,
                x.numerator as f64 / x.denominator as f64
            ))
        )?;

        match info.format {
            Property::Variable => writeln!(writer, "Format Name: Variable")?,
            Property::Constant(f) => {
                writeln!(writer, "Format Name: {}", f.name())?;
                writeln!(writer, "Color Family: {}", f.color_family())?;
                writeln!(
                    writer,
                    "Alpha: {}",
                    if alpha.is_some() { "Yes" } else { "No" }
                )?;
                writeln!(writer, "Sample Type: {}", f.sample_type())?;
                writeln!(writer, "Bits: {}", f.bits_per_sample())?;
                writeln!(writer, "SubSampling W: {}", f.sub_sampling_w())?;
                writeln!(writer, "SubSampling H: {}", f.sub_sampling_h())?;
            }
        }

        Ok(())
    }

    fn print_y4m_header<W: Write>(writer: &mut W, node: &Node) -> Result<(), Error> {
        let info = node.info();

        if let Property::Constant(format) = info.format {
            write!(writer, "YUV4MPEG2 C")?;

            match format.color_family() {
                ColorFamily::Gray => {
                    write!(writer, "mono")?;
                    if format.bits_per_sample() > 8 {
                        write!(writer, "{}", format.bits_per_sample())?;
                    }
                }
                ColorFamily::YUV => {
                    write!(
                        writer,
                        "{}",
                        match (format.sub_sampling_w(), format.sub_sampling_h()) {
                            (1, 1) => "420",
                            (1, 0) => "422",
                            (0, 0) => "444",
                            (2, 2) => "410",
                            (2, 0) => "411",
                            (0, 1) => "440",
                            _ => {
                                return Err(err_msg(
                                    "No y4m identifier exists for the current format"
                                ))
                            }
                        }
                    )?;

                    if format.bits_per_sample() > 8 && format.sample_type() == SampleType::Integer {
                        write!(writer, "p{}", format.bits_per_sample())?;
                    } else if format.sample_type() == SampleType::Float {
                        write!(
                            writer,
                            "p{}",
                            match format.bits_per_sample() {
                                16 => "h",
                                32 => "s",
                                64 => "d",
                                _ => unreachable!(),
                            }
                        )?;
                    }
                }
                _ => return Err(err_msg("No y4m identifier exists for the current format")),
            }

            if let Property::Constant(resolution) = info.resolution {
                write!(writer, " W{} H{}", resolution.width, resolution.height)?;
            } else {
                unreachable!();
            }

            if let Property::Constant(framerate) = info.framerate {
                write!(
                    writer,
                    " F{}:{}",
                    framerate.numerator, framerate.denominator
                )?;
            } else {
                unreachable!();
            }

            #[cfg(feature = "gte-vapoursynth-api-32")]
            let num_frames = info.num_frames;

            #[cfg(not(feature = "gte-vapoursynth-api-32"))]
            let num_frames = {
                if let Property::Constant(num_frames) = info.num_frames {
                    num_frames
                } else {
                    unreachable!();
                }
            };

            write!(writer, " Ip A0:0 XLENGTH={}\n", num_frames)?;

            Ok(())
        } else {
            unreachable!();
        }
    }

    // Checks if the frame is completed, that is, we have the frame and, if needed, its alpha part.
    fn is_completed(entry: &(Option<Frame>, Option<Frame>), have_alpha: bool) -> bool {
        entry.0.is_some() && (!have_alpha || entry.1.is_some())
    }

    fn print_frame<W: Write>(writer: &mut W, frame: &Frame) -> Result<(), Error> {
        const RGB_REMAP: [usize; 3] = [1, 2, 0];

        let format = frame.format();
        for plane in 0..format.plane_count() {
            let plane = if format.color_family() == ColorFamily::RGB {
                RGB_REMAP[plane]
            } else {
                plane
            };

            if let Ok(data) = frame.data(plane) {
                writer.write_all(data)?;
            } else {
                for row in 0..frame.height(plane) {
                    writer.write_all(frame.data_row(plane, row))?;
                }
            }
        }

        Ok(())
    }

    fn print_frames<W: Write>(
        writer: &mut W,
        parameters: &OutputParameters,
        frame: Frame,
        alpha_frame: Option<Frame>,
    ) -> Result<(), Error> {
        if parameters.y4m {
            write!(writer, "FRAME\n").context("Couldn't output the frame header")?;
        }

        print_frame(writer, &frame)?;
        if let Some(alpha_frame) = alpha_frame {
            print_frame(writer, &alpha_frame)?;
        }

        Ok(())
    }

    fn frame_done_callback(
        frame: Result<Frame, GetFrameError>,
        n: usize,
        _node: Node,
        shared_data: Arc<SharedData>,
        alpha: bool,
    ) {
        let parameters = &shared_data.output_parameters;
        let mut state = shared_data.output_state.lock().unwrap();

        match frame {
            Err(error) => {
                state.error = Some((
                    n,
                    err_msg(error.into_inner().to_string_lossy().into_owned()),
                ))
            }
            Ok(frame) => {
                {
                    let entry = state.reorder_map.entry(n).or_insert((None, None));
                    if alpha {
                        entry.1 = Some(frame);
                    } else {
                        entry.0 = Some(frame);
                    }
                }

                if is_completed(&state.reorder_map[&n], parameters.alpha_node.is_some())
                    && state.last_requested_frame < parameters.end_frame
                {
                    // Request one more frame.
                    let shared_data_2 = shared_data.clone();
                    parameters.node.get_frame_async(
                        state.last_requested_frame + 1,
                        move |frame, n, node| {
                            frame_done_callback(frame, n, node, shared_data_2, false)
                        },
                    );

                    if let Some(ref alpha_node) = parameters.alpha_node {
                        let shared_data_2 = shared_data.clone();
                        alpha_node.get_frame_async(
                            state.last_requested_frame + 1,
                            move |frame, n, node| {
                                frame_done_callback(frame, n, node, shared_data_2, true)
                            },
                        );
                    }

                    state.last_requested_frame += 1;
                }

                // Output all completed frames.
                while state
                    .reorder_map
                    .get(&state.next_output_frame)
                    .map(|entry| is_completed(entry, parameters.alpha_node.is_some()))
                    .unwrap_or(false)
                {
                    let next_output_frame = state.next_output_frame;
                    let (frame, alpha_frame) =
                        state.reorder_map.remove(&next_output_frame).unwrap();

                    if state.error.is_none() {
                        if let Err(error) = print_frames(
                            &mut state.output_target,
                            parameters,
                            frame.unwrap(),
                            alpha_frame,
                        ) {
                            state.error = Some((n, error));
                        }
                    }

                    state.next_output_frame += 1;
                }
            }
        }

        if state.next_output_frame == parameters.end_frame + 1 {
            *shared_data.output_done_pair.0.lock().unwrap() = true;
            shared_data.output_done_pair.1.notify_one();
        }
    }

    fn output(
        mut output_target: OutputTarget,
        mut timecodes_file: Option<File>,
        parameters: OutputParameters,
    ) -> Result<(), Error> {
        // Print the y4m header.
        if parameters.y4m {
            if parameters.alpha_node.is_some() {
                return Err(err_msg("Can't apply y4m headers to a clip with alpha"));
            }

            print_y4m_header(&mut output_target, &parameters.node)
                .context("Couldn't write the y4m header")?;
        }

        // Print the timecodes header.
        if let Some(ref mut timecodes_file) = timecodes_file {
            writeln!(timecodes_file, "# timecode format v2")?;
        }

        let initial_requests = cmp::min(
            parameters.requests,
            parameters.end_frame - parameters.start_frame + 1,
        );

        let output_done_pair = (Mutex::new(false), Condvar::new());
        let output_state = Mutex::new(OutputState {
            output_target,
            timecodes_file,
            error: None,
            reorder_map: HashMap::new(),
            last_requested_frame: parameters.start_frame + initial_requests - 1,
            next_output_frame: 0,
        });
        let shared_data = Arc::new(SharedData {
            output_done_pair,
            output_parameters: parameters,
            output_state,
        });

        // Start off by requesting some frames.
        {
            let parameters = &shared_data.output_parameters;
            for n in 0..initial_requests {
                let shared_data_2 = shared_data.clone();
                parameters.node.get_frame_async(n, move |frame, n, node| {
                    frame_done_callback(frame, n, node, shared_data_2, false)
                });

                if let Some(ref alpha_node) = parameters.alpha_node {
                    let shared_data_2 = shared_data.clone();
                    alpha_node.get_frame_async(n, move |frame, n, node| {
                        frame_done_callback(frame, n, node, shared_data_2, true)
                    });
                }
            }
        }

        let &(ref lock, ref cvar) = &shared_data.output_done_pair;
        let mut done = lock.lock().unwrap();
        while !*done {
            done = cvar.wait(done).unwrap();
        }

        let mut state = shared_data.output_state.lock().unwrap();
        if let Some((n, ref msg)) = state.error {
            return Err(err_msg(format!(
                "Failed to retrieve frame {} with error: {}",
                n, msg
            )));
        }

        // Flush the output file.
        state
            .output_target
            .flush()
            .context("Failed to flush the output file")?;

        Ok(())
    }

    pub fn run() -> Result<(), Error> {
        let matches = App::new("vspipe-rs")
            .about("A Rust implementation of vspipe")
            .author("Ivan M. <yalterz@gmail.com>")
            .arg(
                Arg::with_name("arg")
                    .short("a")
                    .long("arg")
                    .takes_value(true)
                    .multiple(true)
                    .number_of_values(1)
                    .value_name("key=value")
                    .display_order(1)
                    .help("Argument to pass to the script environment")
                    .long_help(
                        "Argument to pass to the script environment, \
                         a key with this name and value (bytes typed) \
                         will be set in the globals dict",
                    ),
            )
            .arg(
                Arg::with_name("start")
                    .short("s")
                    .long("start")
                    .takes_value(true)
                    .value_name("N")
                    .display_order(2)
                    .help("First frame to output"),
            )
            .arg(
                Arg::with_name("end")
                    .short("e")
                    .long("end")
                    .takes_value(true)
                    .value_name("N")
                    .display_order(3)
                    .help("Last frame to output"),
            )
            .arg(
                Arg::with_name("outputindex")
                    .short("o")
                    .long("outputindex")
                    .takes_value(true)
                    .value_name("N")
                    .display_order(4)
                    .help("Output index"),
            )
            .arg(
                Arg::with_name("requests")
                    .short("r")
                    .long("requests")
                    .takes_value(true)
                    .value_name("N")
                    .display_order(5)
                    .help("Number of concurrent frame requests"),
            )
            .arg(
                Arg::with_name("y4m")
                    .short("y")
                    .long("y4m")
                    .help("Add YUV4MPEG headers to output"),
            )
            .arg(
                Arg::with_name("timecodes")
                    .short("t")
                    .long("timecodes")
                    .takes_value(true)
                    .value_name("FILE")
                    .display_order(6)
                    .help("Write timecodes v2 file"),
            )
            .arg(
                Arg::with_name("progress")
                    .short("p")
                    .long("progress")
                    .help("Print progress to stderr"),
            )
            .arg(
                Arg::with_name("info")
                    .short("i")
                    .long("info")
                    .help("Show video info and exit"),
            )
            .arg(
                Arg::with_name("version")
                    .short("v")
                    .long("version")
                    .help("Show version info and exit")
                    .conflicts_with_all(&[
                        "info",
                        "progress",
                        "y4m",
                        "arg",
                        "start",
                        "end",
                        "outputindex",
                        "requests",
                        "timecodes",
                        "script",
                        "outfile",
                    ]),
            )
            .arg(
                Arg::with_name("script")
                    .required_unless("version")
                    .index(1)
                    .help("Input .vpy file"),
            )
            .arg(
                Arg::with_name("outfile")
                    .required_unless("version")
                    .index(2)
                    .help("Output file")
                    .long_help(
                        "Output file, use hyphen `-` for stdout \
                         or dot `.` for suppressing any output",
                    ),
            )
            .get_matches();

        // Check --version.
        if matches.is_present("version") {
            return print_version();
        }

        // Open the output files.
        let mut output_target = match matches.value_of_os("outfile").unwrap() {
            x if x == OsStr::new(".") => OutputTarget::Empty,
            x if x == OsStr::new("-") => OutputTarget::Stdout(stdout()),
            path => {
                OutputTarget::File(File::create(path).context("Couldn't open the output file")?)
            }
        };

        let timecodes_file = match matches.value_of_os("timecodes") {
            Some(path) => {
                Some(File::create(path).context("Couldn't open the timecodes output file")?)
            }
            None => None,
        };

        // Create a new VSScript environment.
        let mut environment =
            Environment::new().context("Couldn't create the VSScript environment")?;

        // Parse and set the --arg arguments.
        if let Some(args) = matches.values_of("arg") {
            let mut args_map = OwnedMap::new(API::get().unwrap());

            for arg in args.map(parse_arg) {
                let (name, value) = arg.context("Couldn't parse an argument")?;
                args_map
                    .append_data(name, value.as_bytes())
                    .context("Couldn't append an argument value")?;
            }

            environment
                .set_variables(&args_map)
                .context("Couldn't set arguments")?;
        }

        // Evaluate the script.
        environment
            .eval_file(
                matches.value_of("script").unwrap(),
                EvalFlags::SetWorkingDir,
            )
            .context("Script evaluation failed")?;

        // Get the output node.
        let output_index = matches
            .value_of("outputindex")
            .map(str::parse)
            .unwrap_or(Ok(0))
            .context("Couldn't convert the output index to an integer")?;

        #[cfg(feature = "gte-vsscript-api-31")]
        let (node, alpha_node) = environment.get_output(output_index).context(format!(
            "Couldn't get the output node at index {}",
            output_index
        ))?;
        #[cfg(not(feature = "gte-vsscript-api-31"))]
        let (node, alpha_node) = (
            environment.get_output(output_index).context(format!(
                "Couldn't get the output node at index {}",
                output_index
            ))?,
            None::<Node>,
        );

        if matches.is_present("info") {
            print_info(&mut output_target, &node, alpha_node.as_ref())
                .context("Couldn't print info to the output file")?;

            output_target
                .flush()
                .context("Couldn't flush the output file")?;
        } else {
            let num_frames = {
                let info = node.info();

                if let Property::Variable = info.format {
                    return Err(err_msg("Cannot output clips with varying format"));
                }
                if let Property::Variable = info.resolution {
                    return Err(err_msg("Cannot output clips with varying dimensions"));
                }
                if let Property::Variable = info.framerate {
                    return Err(err_msg("Cannot output clips with varying framerate"));
                }

                #[cfg(feature = "gte-vapoursynth-api-32")]
                let num_frames = info.num_frames;

                #[cfg(not(feature = "gte-vapoursynth-api-32"))]
                let num_frames = {
                    match info.num_frames {
                        Property::Variable => {
                            // TODO: make it possible?
                            return Err(err_msg("Cannot output clips with unknown length"));
                        }
                        Property::Constant(x) => x,
                    }
                };

                num_frames
            };

            let start_frame = matches
                .value_of("start")
                .map(str::parse::<i32>)
                .unwrap_or(Ok(0))
                .context("Couldn't convert the start frame to an integer")?;
            let end_frame = matches
                .value_of("end")
                .map(str::parse::<i32>)
                .unwrap_or(Ok(num_frames as i32 - 1))
                .context("Couldn't convert the end frame to an integer")?;

            // Check if the input start and end frames make sense.
            if start_frame < 0 || end_frame < start_frame || end_frame as usize >= num_frames {
                return Err(err_msg(format!(
                    "Invalid range of frames to output specified:\n\
                     first: {}\n\
                     last: {}\n\
                     clip length: {}\n\
                     frames to output: {}",
                    start_frame,
                    end_frame,
                    num_frames,
                    end_frame
                        .checked_sub(start_frame)
                        .and_then(|x| x.checked_add(1))
                        .map(|x| format!("{}", x))
                        .unwrap_or_else(|| "<overflow>".to_owned())
                )));
            }

            let requests = {
                let requests = matches
                    .value_of("requests")
                    .map(str::parse::<usize>)
                    .unwrap_or(Ok(0))
                    .context("Couldn't convert the request count to an unsigned integer")?;

                if requests == 0 {
                    environment.get_core().unwrap().info().num_threads
                } else {
                    requests
                }
            };

            let y4m = matches.is_present("y4m");
            let progress = matches.is_present("progress");

            output(
                output_target,
                timecodes_file,
                OutputParameters {
                    node,
                    alpha_node,
                    start_frame: start_frame as usize,
                    end_frame: end_frame as usize,
                    requests,
                    y4m,
                    progress,
                },
            ).context("Couldn't output the frames")?;
        }

        Ok(())
    }
}

#[cfg(not(all(feature = "vsscript-functions",
              any(feature = "vapoursynth-functions", feature = "gte-vsscript-api-32"))))]
mod inner {
    use super::*;

    pub fn run() -> Result<(), Error> {
        Err(err_msg(
            "This example requires the `vsscript-functions` and either `vapoursynth-functions` or \
             `vsscript-api-32` features.",
        ))
    }
}

fn main() {
    if let Err(err) = inner::run() {
        eprintln!("Error: {}", err.cause());

        for cause in err.causes().skip(1) {
            eprintln!("Caused by: {}", cause);
        }

        eprintln!("{}", err.backtrace());
    }
}
