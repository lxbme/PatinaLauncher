pub mod detect;

pub use detect::{
    JavaInstall, detect_all, parse_major, parse_properties_output, parse_release_file, probe,
};
