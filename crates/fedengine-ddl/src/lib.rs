//! Fail-closed SQL analysis, source binding and controlled index templates.
mod analysis;
mod binding;
mod index;
mod model;
mod parameters;

pub use analysis::Analyzer;
pub use index::drop_index_name;
pub use model::{Analysis, IndexIntent, IndexOwner, Kind, ObjectRef};
pub use parameters::{cast_parameters, parameter_hints};
