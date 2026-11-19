pub mod chapters;
pub mod error;
pub mod exec;
pub mod extract;
pub mod model;
pub mod plan;
pub mod preview;
pub mod probe;
pub mod sync;
pub mod tools;
pub mod verify;

mod sys;

pub use chapters::resolve as resolve_chapters;
pub use error::{Error, Result};
pub use exec::{run_extract, run_mux, ExecEvent, MuxOutcome};
pub use extract::{default_track_path, ExtractContainer, ExtractPlan};
pub use model::{
    format_duration, format_size, language_name, AttachmentInfo, Backend, Chapter, MediaInfo,
    OutputContainer, StreamInfo, StreamKind,
};
pub use plan::{
    default_output_path, parse_ranges, parse_time_ms, paths_equal, prepare, validate, CommandSpec,
    ExtraInput, ExtraTrack, KeepTrack, MuxPlan, PreparedCommand, SplitSpec,
};
pub use sync::{estimate_delay, OffsetEstimate};
pub use tools::ToolPaths;
pub use verify::{verify_extract, verify_output, VerifyReport};
