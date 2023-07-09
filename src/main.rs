use std::{
    path::{
        PathBuf,
        Path,
    },
    fs::{
        rename,
        read_dir,
    },
    io,
};
use clap::{
    self,
    Parser,
};
use ffmpeg_next as ffmpeg;
use ffmpeg::{
    Rational,
};
use chrono::{
    DateTime,
    Utc,
    Local,
};
use serde_json::{
    Value,
    Map,
};
use tinytemplate::TinyTemplate;

#[derive(Debug)]
enum Error {
    Message(String),
    ChronoFormatParseError(chrono::format::ParseError),
    FfmpegError(ffmpeg::Error),
    TinyTemplateError(tinytemplate::error::Error),
    IoError(std::io::Error),
}

impl From<chrono::format::ParseError> for Error {
    fn from(err: chrono::format::ParseError) -> Error {
        Error::ChronoFormatParseError(err)
    }
}

impl From<ffmpeg::Error> for Error {
    fn from(err: ffmpeg::Error) -> Error {
        Error::FfmpegError(err)
    }
}

impl From<tinytemplate::error::Error> for Error {
    fn from(err: tinytemplate::error::Error) -> Error {
        Error::TinyTemplateError(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::IoError(err)
    }
}

#[derive(clap::Parser, Debug)]
#[clap(group(clap::ArgGroup::new("path").required(true).args(&["dir", "file"])))]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long)]
    template: String,

    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    dir: Option<PathBuf>,

    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    file: Option<PathBuf>,

    #[arg(long, default_value = "%Y%m%d%H%M%S")]
    datetime_format: String,

    #[arg(long, action = clap::ArgAction::SetTrue)]
    run: bool,
}

fn main() -> Result<(), Error> {
    let args = Args::parse();

    let (path, for_dir) = if let Some(path) = &args.dir {
        (path, true)
    } else if let Some(path) = &args.file {
        (path, false)
    } else {
        panic!("BUG: dir or file must be set by the clap library")
    };

    let mut tt = TinyTemplate::new();
    tt.add_template("main", &args.template)?;

    ffmpeg::init()?;
    if for_dir {
        process_dir(&path, &args, &tt)
    } else {
        process_file(&path, &args, &tt)
    }
}

fn process_dir(path: &Path, args: &Args, tt: &TinyTemplate) -> Result<(), Error> {
    if !path.is_dir() {
        return Err(Error::Message(format!("Couldn't get parent dir from: {:?}", path)));
    }
    let paths = read_dir(path)?.filter_map(|e| e.ok()).map(|e| e.path()).collect::<Vec<_>>();
    for child_path in paths {
        if child_path.is_dir() {
            process_dir(&child_path, args, tt)?;
        } else {
            match process_file(&child_path, args, tt) {
                Err(err) => {
                    eprintln!("Error occurred in the path: {:?}\n{:?}", child_path, err);
                },
                _ => (),
            }
        }
    }
    Ok(())
}

fn process_file(path: &Path, args: &Args, tt: &TinyTemplate) -> Result<(), Error> {
    if let Some(filename) = path.file_name() {
        let ctx = ffmpeg::format::input(&path);
        let Ok(ctx) = ctx else {
            // provably non video file
            return Ok(())
        };

        let mut metadata_value = get_metadata_value(ctx, args)?;
        let filename = filename.to_string_lossy().into_owned();

        let Some(metadata_map) = metadata_value.as_object_mut() else {
            panic!("metadata should be an json object");
        };

        metadata_map.insert("org".into(), filename.clone().into());
        metadata_map.insert("original".into(), filename.clone().into());
        metadata_map.insert("original_filename".into(), filename.clone().into());

        let Some(parent_dir) = path.parent() else {
            return Err(Error::Message(format!("Couldn't get parent dir from: {:?}", path)));
        };

        let new_filename = tt.render("main", &metadata_value)?;
        let new_path = parent_dir.join(&new_filename);

        if new_path.exists() {
            return Err(Error::Message(format!("File already exists at new path: {:?}", new_path)));
        }

        if args.run {
            rename(&path, &new_path)?;
            eprintln!("rename {:?} {:?}", filename, new_filename);
        } else {
            eprintln!("DRY_RUN: rename {:?} {:?}", filename, new_filename);
        }
    }

    Ok(())
}

fn get_metadata_value(ctx: ffmpeg::format::context::input::Input, args: &Args) -> Result<Value, Error> {
    let mut root_map = Map::new();
    for (k, v) in ctx.metadata().iter() {
        let value = if k == "creation_time" {
            format_datetime(v.into(), &args.datetime_format)?
        } else {
            v.into()
        };

        if k == "creation_time" {
            root_map.insert("ct".into(), value.clone());
        }

        root_map.insert(k.into(), value);
    }

    fn insert_rational_value(map: &mut Map<String, Value>, key: &str, rational: Rational) {
        map.insert(key.into(), Value::Array(vec![
            rational.numerator().into(),
            rational.denominator().into(),
        ]));
        map.insert((key.to_string() + "_str").into(), format!("{}", rational).into());
        map.insert((key.to_string() + "_float").into(), f64::from(rational).into());
    }

    let mut streams: Vec<Value> = Vec::new();
    for stream in ctx.streams() {
        let mut stream_map = Map::new();
        stream_map.insert("index".into(), stream.index().into());
        insert_rational_value(&mut stream_map, "time_base", stream.time_base());
        stream_map.insert("start_time".into(), stream.start_time().into());
        stream_map.insert("duration_stream_timebase".into(), stream.duration().into());
        stream_map.insert("duration_in_sec".into(), (stream.duration() as f64 * f64::from(stream.time_base())).into());
        stream_map.insert("frames".into(), stream.frames().into());
        stream_map.insert("disposition".into(), format!("{:?}", stream.disposition()).into());
        stream_map.insert("discard".into(), format!("{:?}", stream.discard()).into());
        insert_rational_value(&mut stream_map, "rate", stream.rate());

        let codec = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;

        stream_map.insert("codec_medium".into(), format!("{:?}", codec.medium()).into());
        stream_map.insert("codec".into(), format!("{:?}", codec.id()).into());

        if codec.medium() == ffmpeg::media::Type::Video {
            if let Ok(video) = codec.decoder().video() {
                stream_map.insert("bit_rate".into(), video.bit_rate().into());
                stream_map.insert("max_bit_rate".into(), video.max_bit_rate().into());
                stream_map.insert("delay".into(), video.delay().into());
                stream_map.insert("width".into(), video.width().into());
                stream_map.insert("height".into(), video.height().into());
                stream_map.insert("format".into(), format!("{:?}", video.format()).into());
                stream_map.insert("has_b_frames".into(), video.has_b_frames().into());
                insert_rational_value(&mut stream_map, "aspect_ratio", video.aspect_ratio());
                stream_map.insert("color_space".into(), format!("{:?}", video.color_space()).into());
                stream_map.insert("color_range".into(), format!("{:?}", video.color_range()).into());
                stream_map.insert("color_primaries".into(), format!("{:?}", video.color_primaries()).into());
                stream_map.insert("color_transfer_characteristic".into(), format!("{:?}", video.color_transfer_characteristic()).into());
                stream_map.insert("chroma_location".into(), format!("{:?}", video.chroma_location()).into());
                stream_map.insert("references".into(), video.references().into());
                stream_map.insert("intra_dc_precision".into(), video.intra_dc_precision().into());
            }
        } else if codec.medium() == ffmpeg::media::Type::Audio {
            if let Ok(audio) = codec.decoder().audio() {
                stream_map.insert("bit_rate".into(), audio.bit_rate().into());
                stream_map.insert("max_bit_rate".into(), audio.max_bit_rate().into());
                stream_map.insert("delay".into(), audio.delay().into());
                stream_map.insert("rate".into(), audio.rate().into());
                stream_map.insert("channels".into(), audio.channels().into());
                stream_map.insert("format".into(), format!("{:?}", audio.format()).into());
                stream_map.insert("frames".into(), audio.frames().into());
                stream_map.insert("align".into(), audio.align().into());
                stream_map.insert("channel_layout".into(), format!("{:?}", audio.channel_layout()).into());
            }
        }
        streams.push(stream_map.into());
    }

    if let Some(stream) = ctx.streams().best(ffmpeg::media::Type::Video) {
        if let Some(video_stream_value) = streams.get(stream.index()) {
            root_map.insert("v".into(), video_stream_value.clone());
            root_map.insert("video".into(), video_stream_value.clone());
        }
    }

    if let Some(stream) = ctx.streams().best(ffmpeg::media::Type::Audio) {
        if let Some(audio_stream_value) = streams.get(stream.index()) {
            root_map.insert("a".into(), audio_stream_value.clone());
            root_map.insert("audio".into(), audio_stream_value.clone());
        }
    }

    if let Some(stream) = ctx.streams().best(ffmpeg::media::Type::Subtitle) {
        if let Some(subtitle_stream_value) = streams.get(stream.index()) {
            root_map.insert("s".into(), subtitle_stream_value.clone());
            root_map.insert("subtitle".into(), subtitle_stream_value.clone());
        }
    }

    root_map.insert("streams".into(), streams.into());
    Ok(Value::Object(root_map))
}

fn format_datetime<'a>(value: Value, fmt: &'a str) -> Result<Value, Error> {
    let Some(parse_target_str) = value.as_str() else {
        return Err(Error::Message("Value couldn't be converted to a string.".into()));
    };
    let dt = parse_target_str.parse::<DateTime<Utc>>()?;
    let dt: DateTime<Local> = dt.into();
    Ok(Value::String(format!("{}", dt.format(fmt))))
}

