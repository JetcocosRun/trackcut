pub mod error;
pub mod exec;
pub mod extract;
pub mod model;
pub mod plan;
pub mod preview;
pub mod probe;
pub mod tools;
pub mod verify;

mod sys;

pub use error::{Error, Result};
pub use exec::{run_extract, run_mux, ExecEvent, MuxOutcome};
pub use extract::{default_track_path, ExtractContainer, ExtractPlan};
pub use model::{
    format_duration, format_size, language_name, AttachmentInfo, Backend, MediaInfo,
    OutputContainer, StreamInfo, StreamKind,
};
pub use plan::{
    default_output_path, paths_equal, prepare, validate, CommandSpec, ExtraInput, ExtraTrack,
    KeepTrack, MuxPlan, PreparedCommand,
};
pub use tools::ToolPaths;
pub use verify::{verify_extract, verify_output, VerifyReport};
